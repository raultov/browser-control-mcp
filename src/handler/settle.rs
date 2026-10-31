//! Waiting for the page to react to an input action (`click_element`,
//! `hover`, `fill_input`, `fill_form`, `press_key`), on every browser: the
//! events are dispatched in a few milliseconds, but the page answers them
//! later (a fetch, then a DOM update), so a read right after the action saw
//! the old state. The page is settled once its DOM stayed unchanged for
//! [`QUIET`] and the requests the action started finished, at most [`MAX`];
//! an action nothing reacts to costs [`QUIET`] and adds nothing to the reply.

use crate::handler::NetworkState;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// How long the DOM must stay unchanged to count as settled.
pub(crate) const QUIET: Duration = Duration::from_millis(50);

/// Longest wait for the page to settle.
pub(crate) const MAX: Duration = Duration::from_millis(1000);

/// How often the requests an action started are checked.
const POLL: Duration = Duration::from_millis(20);

/// How many times the page may not answer the watch (its document was
/// being replaced) before it is given up.
const MAX_MISSES: u32 = 3;

/// The page an action ran on, to watch it settle.
pub(crate) enum Page {
    /// A Chrome tab (CDP).
    Chrome(CdpTarget),
    /// A Firefox tab (WebDriver BiDi).
    Firefox(bidi_browser_lite::Tab),
}

/// What the page's DOM did while it was watched (the snippet's answer).
#[derive(Debug, Default, PartialEq, serde::Deserialize)]
struct Quiet {
    mutations: u64,
    settled: bool,
}

/// How the page reacted to an action.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Reaction {
    /// DOM mutations seen (style changes, often animations, left out).
    pub(crate) mutations: u64,
    /// Requests the action started.
    pub(crate) requests: usize,
    /// Time spent watching.
    pub(crate) waited_ms: u64,
    /// Whether the page was quiet before [`MAX`].
    pub(crate) settled: bool,
}

impl Reaction {
    /// The sentence a reply adds, or `None` when nothing reacted.
    pub(crate) fn note(&self) -> Option<String> {
        let mut what = Vec::new();
        if self.mutations > 0 {
            what.push("the DOM changed".to_string());
        }
        if self.requests > 0 {
            what.push(format!("{} request(s)", self.requests));
        }
        if what.is_empty() {
            return None;
        }
        let (what, ms) = (what.join(", "), self.waited_ms);
        Some(if self.settled {
            format!("The page reacted ({what}) and settled after {ms} ms.")
        } else {
            format!(
                "The page was still changing after {ms} ms ({what}): use wait_for for the state you expect."
            )
        })
    }
}

/// The snippet that watches the DOM until it stays unchanged for `quiet`
/// (at most `max`): `{mutations, settled}`.
fn quiet_js(quiet: Duration, max: Duration) -> String {
    let (quiet, max) = (quiet.as_millis(), max.as_millis());
    format!(
        "new Promise(resolve => {{ const quiet = {quiet}, max = {max}, start = performance.now(); let last = start, mutations = 0; \
         const observer = new MutationObserver(records => {{ for (const r of records) if (r.attributeName !== 'style') {{ mutations++; last = performance.now(); }} }}); \
         observer.observe(document, {{ subtree: true, childList: true, attributes: true, characterData: true }}); \
         const check = () => {{ const now = performance.now(), settled = now - last >= quiet; \
         if (settled || now - start >= max) {{ observer.disconnect(); resolve({{ mutations, settled }}); }} \
         else setTimeout(check, Math.min(quiet - (now - last), max - (now - start)) + 1); }}; \
         setTimeout(check, Math.min(quiet, max)); }})"
    )
}

impl Page {
    /// Watches the DOM for at most `max`; `None` when the page did not
    /// answer (it navigated away, or a dialog blocks it).
    async fn quiet(&self, max: Duration) -> Option<Quiet> {
        let expression = quiet_js(QUIET, max);
        let value = match self {
            Self::Chrome(target) => {
                let params =
                    json!({"expression": expression, "awaitPromise": true, "returnByValue": true});
                let reply = target.send_raw_command("Runtime.evaluate", params).await;
                // Errors are dropped at once: the tool error type is not `Send`.
                let result = reply.ok()?.result.unwrap_or_default();
                crate::handler::snippet_outcome(result).ok()?
            }
            Self::Firefox(tab) => crate::backend::bidi::page::evaluate(tab, &expression)
                .await
                .ok()?,
        };
        serde_json::from_value(value).ok()
    }
}

/// Watches `page` settle after an action, given the requests its log held
/// before (`known`); `None` when it could not be watched.
pub(crate) async fn settle(
    page: &Page,
    network: &Arc<Mutex<NetworkState>>,
    known: &HashSet<String>,
) -> Option<Reaction> {
    let start = Instant::now();
    let deadline = start + MAX;
    let mut reaction = Reaction::default();
    let mut misses = 0;
    // The DOM settles, then the requests the action started finish (and
    // their answers change the DOM again): rounds until a quiet one.
    loop {
        let budget = deadline.saturating_duration_since(Instant::now());
        let Some(quiet) = page.quiet(budget).await else {
            // A navigation replaced the document: watch the new one.
            misses += 1;
            if misses > MAX_MISSES || Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(POLL).await;
            continue;
        };
        reaction.mutations += quiet.mutations;
        let before = reaction.requests;
        let (seen, loading) = network.lock().await.new_since(known);
        reaction.requests = seen;
        reaction.settled = quiet.settled && seen == before && loading == 0;
        let more = quiet.settled && !reaction.settled;
        if !more || !requests_done(network, known, deadline).await {
            reaction.waited_ms = elapsed_ms(start);
            return Some(reaction);
        }
    }
}

/// Milliseconds since `start`.
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Waits until the requests not in `known` finished: `false` when some
/// were still loading at `deadline`.
async fn requests_done(
    network: &Arc<Mutex<NetworkState>>,
    known: &HashSet<String>,
    deadline: Instant,
) -> bool {
    while Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
        if network.lock().await.new_since(known).1 == 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::NetworkRequest;

    fn request(url: &str) -> NetworkRequest {
        NetworkRequest {
            url: url.into(),
            method: "GET".into(),
            ..Default::default()
        }
    }

    /// ISSUE-21: a reply says how the page reacted, and nothing when it
    /// did not.
    #[test]
    fn the_note_says_how_the_page_reacted() {
        assert_eq!(Reaction::default().note(), None);
        let settled = Reaction {
            mutations: 4,
            requests: 1,
            waited_ms: 120,
            settled: true,
        };
        assert_eq!(
            settled.note().as_deref(),
            Some("The page reacted (the DOM changed, 1 request(s)) and settled after 120 ms.")
        );
        let busy = Reaction {
            mutations: 9,
            requests: 0,
            waited_ms: 1003,
            settled: false,
        };
        assert_eq!(
            busy.note().as_deref(),
            Some(
                "The page was still changing after 1003 ms (the DOM changed): use wait_for for the state you expect."
            )
        );
    }

    /// ISSUE-21: only requests the action started count, and they are
    /// waited for until they finish or fail.
    #[tokio::test]
    async fn requests_started_by_the_action_are_waited_for() {
        // Given a stream open before the action, and a request it started
        let mut state = NetworkState::default();
        state.insert_request("sse".into(), request("http://a/events"));
        let known = state.known_ids();
        state.insert_request("api".into(), request("http://a/api"));
        assert_eq!(state.new_since(&known), (1, 1));
        let network = Arc::new(Mutex::new(state));
        // When the request finishes 100 ms later
        let finishing = network.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut state = finishing.lock().await;
            if let Some(api) = state.find_request_mut("api") {
                api.finished = true;
            }
        });
        // Then the wait ends then, though the stream is still open
        let start = Instant::now();
        let deadline = start + MAX;
        assert!(requests_done(&network, &known, deadline).await);
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert!(start.elapsed() < MAX);
        assert_eq!(network.lock().await.new_since(&known), (1, 0));
        // And a request that never ends is given up at the deadline
        network
            .lock()
            .await
            .insert_request("slow".into(), request("http://a/slow"));
        let soon = Instant::now() + Duration::from_millis(60);
        assert!(!requests_done(&network, &known, soon).await);
    }

    /// ISSUE-21: the snippet ignores style changes (animations), so a
    /// page that animates still settles.
    #[test]
    fn the_snippet_ignores_style_changes() {
        let js = quiet_js(QUIET, MAX);
        assert!(js.contains("attributeName !== 'style'"), "{js}");
        assert!(js.contains("const quiet = 50, max = 1000"), "{js}");
    }
}
