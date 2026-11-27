//! A tab's console buffer: what the page, its frames and its workers
//! logged, and the errors nothing caught. Browsers report one message over
//! several feeds (a CDP session per target; on Firefox, BiDi and the
//! debugger port's watcher), and the buffer holds three rules for them, so
//! `get_console_logs` reads the same whatever the feeds do:
//!
//! 1. **Once.** A message is listed from the feed of the target that
//!    logged it. What another feed says of it is a copy, told by what the
//!    browser states (never by its text alone) and dropped or merged: the
//!    tab's `Log` copy of a watched worker's message names the worker
//!    ([`LogState::is_watched_worker_copy`]); a worker's relay of an error
//!    one of its workers threw carries no thrown value ([`is_relayed`]);
//!    Firefox's two copies of a page error carry the same millisecond and
//!    first line, and are paired oldest first, each once
//!    ([`LogState::push_deduplicated`]).
//! 2. **In order.** The buffer is sorted by the browser's timestamp,
//!    whatever order the feeds deliver in ([`LogState::place`]).
//! 3. **As thrown.** An uncaught error's text is the thrown value printed
//!    as the console prints any value ([`arg_text`]).

pub mod get_console_logs;

use crate::handler::MAX_CONSOLE_MESSAGES;
use crate::util::json::{opt_string, str_field, string_or_empty};
use cdp_browser_lite::WsResponse;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Default, ::serde::Serialize, ::serde::Deserialize)]
pub struct ConsoleMessage {
    pub source: String,
    pub level: String,
    pub text: String,
    pub timestamp: f64,
    pub url: Option<String>,
    pub line_number: Option<i64>,
    /// Which feed reported it (for de-duplication; not part of the reply).
    #[serde(skip)]
    pub(crate) channel: Channel,
    /// The global that logged it, when the feed names it (Firefox: a BiDi
    /// realm, a watcher window id): a cross-site navigation tells the old
    /// document's late messages from the new one's first ones by it (#206).
    #[serde(skip)]
    pub(crate) document: Option<String>,
    /// Whether it reports an error the page's code threw that nothing
    /// caught (an uncaught exception, an unhandled rejection), as the feed
    /// says it: counted for the input actions that ran meanwhile (#210).
    #[serde(skip)]
    pub(crate) uncaught: bool,
    /// The kind of target that logged it, when not the tab's page, its
    /// frames or dedicated workers: `service_worker` (Chrome, #211); its
    /// line says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
}

/// The feed a console message came from. Firefox reports some messages
/// on both: BiDi `log.entryAdded` and the RDP watcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Channel {
    /// The browser's main protocol (CDP, or BiDi on Firefox).
    #[default]
    Primary,
    /// Firefox's RDP watcher resources.
    Watcher,
    /// Both of Firefox's feeds: its two copies, merged. It pairs with no
    /// further copy.
    Both,
}

impl Channel {
    /// The feed whose copy of a message of this one is still awaited;
    /// none once both came.
    fn other(self) -> Option<Self> {
        match self {
            Self::Primary => Some(Self::Watcher),
            Self::Watcher => Some(Self::Primary),
            Self::Both => None,
        }
    }
}

/// Whether two copies of one message from Firefox's two feeds can be
/// stamped `a` and `b` (ms since the epoch): both read the stamp of the
/// one error Firefox recorded, the watcher with its microseconds and BiDi
/// cut to the millisecond (measured on Firefox 160, 700 pairs: page and
/// worker errors, rejections, rejected cookies, CORS and CSP reports, all
/// in the same millisecond). Messages with the same text stamped in
/// different milliseconds are two, and stay so (#218: a tolerance of 1 ms
/// paired an error with its neighbour's copy; one of 2 s could drop one).
fn same_instant(a: f64, b: f64) -> bool {
    a > 0.0 && a.floor().to_bits() == b.floor().to_bits()
}

/// Most characters kept of a console message's text: a page can log
/// megabytes, which `get_console_logs` would return whole.
const CONSOLE_TEXT_MAX: usize = 10_000;

/// `msg` with its text within [`CONSOLE_TEXT_MAX`].
fn bounded(mut msg: ConsoleMessage) -> ConsoleMessage {
    msg.text = crate::util::text::capped(&msg.text, CONSOLE_TEXT_MAX);
    msg
}

#[derive(Default)]
pub(crate) struct LogState {
    pub messages: VecDeque<ConsoleMessage>,
    /// Site of the tab's current document (`util::url::site`), once known.
    site: Option<String>,
    /// Says what the last cross-site navigation cleared, until it is read.
    cleared_note: Option<String>,
    /// When the last cross-site document committed (ms since the epoch):
    /// messages of the old site arriving late are dropped.
    cleared_before: Option<f64>,
    /// The documents (see [`ConsoleMessage::document`]) whose messages the
    /// last cross-site navigation cleared: their late ones are dropped too,
    /// whenever they were stamped.
    old_documents: std::collections::HashSet<String>,
    /// The last navigation Firefox started in the tab (its id) and when
    /// (ms since the epoch), until it commits.
    started: Option<(String, f64)>,
    /// The problems Chrome diagnosed in the tab (`Audits`, ISSUE-60).
    pub(crate) issues: crate::handler::issues::IssueBuffer,
    /// Firefox: by issue code, the time up to which `get_issues` with
    /// `clear` returned that code's issues (they are read from messages).
    pub(crate) issues_cleared_at: std::collections::BTreeMap<String, f64>,
    /// Same-document navigations of the tab (history API, fragment) so
    /// far, and the URL the last reached, so an action can say it changed
    /// the URL (ISSUE-81).
    pub(crate) same_document: u64,
    pub(crate) same_document_url: Option<String>,
    /// Chrome's id of the tab's main frame, once a navigation named it.
    main_frame: Option<String>,
    /// Target ids of the workers watched in their own session (Chrome, see
    /// `frame_watch`): the tab's `Log` copies of their console messages are
    /// dropped. At most `frame_watch::MAX_WATCHED` (one per watched
    /// target).
    workers: Vec<String>,
    /// How many uncaught errors (see [`ConsoleMessage::uncaught`]) the tab
    /// raised so far, each counted once, whichever feeds report it: an
    /// input action says how many it saw (#210). Never lowered: a
    /// navigation or `clear` empties the buffer, not the count.
    pub(crate) uncaught: u64,
    /// Uncaught errors whose `cause` chain is being read (Chrome, see
    /// [`CauseLookup`]): `get_console_logs` waits for them, bounded. Each
    /// is counted by a [`Pending`] it carries, so the count goes down
    /// however the lookup ends.
    pub(crate) causes_pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl LogState {
    /// Notes a same-document navigation of the tab to `url`.
    pub(crate) fn note_same_document(&mut self, url: &str) {
        self.same_document += 1;
        self.same_document_url = Some(url.to_string());
    }

    /// Notes that the worker of target `target_id` reports its console
    /// itself.
    pub(crate) fn watch_worker(&mut self, target_id: &str) {
        self.workers.push(target_id.to_string());
    }

    /// Forgets the watch of the worker of target `target_id` (it detached).
    pub(crate) fn forget_worker(&mut self, target_id: &str) {
        if let Some(at) = self.workers.iter().position(|w| w == target_id) {
            self.workers.remove(at);
        }
    }

    /// Whether the worker of target `target_id` is watched.
    pub(crate) fn watches_worker(&self, target_id: &str) -> bool {
        self.workers.iter().any(|w| w == target_id)
    }

    /// Whether the event (`method`, `params`) is the tab's `Log` copy of a
    /// message a watched worker reports in its own session. The copy names
    /// the worker by its target id (`workerId`); its URL is the script that
    /// logged it, one the worker imported too (#207), and its text differs
    /// (objects as `[object Object]`, errors without their stack).
    fn is_watched_worker_copy(&self, method: &str, params: &Value) -> bool {
        method == "Log.entryAdded"
            && params
                .pointer("/entry/workerId")
                .and_then(Value::as_str)
                .is_some_and(|worker| self.watches_worker(worker))
    }

    pub(crate) fn push_message(&mut self, msg: ConsoleMessage) {
        self.push_bounded(bounded(msg));
    }

    /// Pushes a message whose text is already bounded.
    fn push_bounded(&mut self, msg: ConsoleMessage) {
        self.uncaught += u64::from(msg.uncaught);
        self.place(msg);
    }

    /// Puts `msg` where its timestamp says, after the messages stamped the
    /// same: the feeds of a tab (a session per frame and worker; Firefox's
    /// two connections) deliver in an order of their own, and the buffer
    /// is read oldest first. A message without a stamp goes last, and
    /// none is put before it. The oldest message makes room.
    fn place(&mut self, msg: ConsoleMessage) {
        let later = |m: &ConsoleMessage| m.timestamp > msg.timestamp;
        let after = match msg.timestamp > 0.0 {
            true => self.messages.iter().rev().take_while(|m| later(m)).count(),
            false => 0,
        };
        self.messages.insert(self.messages.len() - after, msg);
        if self.messages.len() > MAX_CONSOLE_MESSAGES {
            self.messages.pop_front();
        }
    }

    /// Where the copy of `msg` Firefox's other feed pushed first is, if
    /// any still waits for it: the oldest message of that feed with its
    /// stamp (see [`same_instant`]) and first line. Each feed delivers in
    /// order, so the oldest copy waiting is this message's: of errors
    /// thrown in one millisecond, the newest was taken, which listed them
    /// newest first (#218). The buffer is sorted, so the search ends at
    /// the first older message, however many came since (a lookback of 64
    /// messages listed twice an error whose first copy was further back).
    fn twin_of(&self, msg: &ConsoleMessage) -> Option<usize> {
        let other = msg.channel.other()?;
        let older =
            |m: &ConsoleMessage| m.timestamp > 0.0 && m.timestamp.floor() < msg.timestamp.floor();
        let recent = self.messages.iter().enumerate().rev();
        recent
            .take_while(|(_, m)| !older(m))
            .filter(|(_, m)| {
                m.channel == other
                    && same_instant(m.timestamp, msg.timestamp)
                    && twin_key(&m.text) == twin_key(&msg.text)
            })
            .last()
            .map(|(at, _)| at)
    }

    /// Pushes a message that another feed may report too (Firefox): with
    /// the copy the other feed already pushed (see [`Self::twin_of`]) it is
    /// kept once, as the watcher gives it (its labels and stack say more),
    /// and pairs with no later copy.
    pub(crate) fn push_deduplicated(&mut self, msg: ConsoleMessage) {
        let before = self
            .cleared_before
            .is_some_and(|at| msg.timestamp > 0.0 && msg.timestamp < at);
        let old = msg
            .document
            .as_ref()
            .is_some_and(|document| self.old_documents.contains(document));
        if before || old {
            return;
        }
        let msg = bounded(msg);
        let first = self.twin_of(&msg).and_then(|at| self.messages.remove(at));
        let Some(first) = first else {
            self.push_bounded(msg);
            return;
        };
        // Counted once: when the first copy said so, or now.
        self.uncaught += u64::from(msg.uncaught && !first.uncaught);
        let uncaught = msg.uncaught || first.uncaught;
        let kept = match msg.channel {
            Channel::Watcher => msg,
            _ => first,
        };
        self.place(ConsoleMessage {
            uncaught,
            channel: Channel::Both,
            ..kept
        });
    }

    /// Notes that Firefox started navigation `navigation` in the tab at `at`
    /// (ms since the epoch). Its start bounds the new document's messages:
    /// Firefox stamps the commit after the new page's first scripts ran, so
    /// their messages were older than it and cleared with the old site's
    /// (#206).
    pub(crate) fn on_navigation_started(&mut self, navigation: &str, at: f64) {
        self.started = Some((navigation.to_string(), at));
    }

    /// Firefox committed navigation `navigation` at `url` (stamped
    /// `committed`): [`Self::on_document_committed`] from when it started,
    /// when that was noted, else from its commit.
    pub(crate) fn on_navigation_committed(
        &mut self,
        url: &str,
        navigation: Option<&str>,
        committed: Option<f64>,
    ) {
        let started = self
            .started
            .take()
            .filter(|(id, _)| Some(id.as_str()) == navigation)
            .map(|(_, at)| at);
        self.on_document_committed(url, started.or(committed));
    }

    /// A new top-level document committed at `url` (its navigation began at
    /// `at`, ms since the epoch, when the browser says). Moving to another
    /// site clears the buffer (the old site's messages would bury the new
    /// page's); same-site navigations and reloads keep it. Same-document
    /// navigations (fragment, history API) never get here. With `at`,
    /// messages stamped at or after it stay (Firefox reports some over
    /// another connection, which can overtake the commit), unless a document
    /// that logged before it logged them; later ones stamped before it, or
    /// from such a document, are dropped.
    pub(crate) fn on_document_committed(&mut self, url: &str, at: Option<f64>) {
        let site = crate::util::url::site(url);
        if let Some(previous) = self.site.as_deref()
            && previous != site
        {
            let before = self.messages.len();
            self.old_documents = match at {
                Some(at) => {
                    let old: std::collections::HashSet<String> = self
                        .messages
                        .iter()
                        .filter(|m| m.timestamp < at)
                        .filter_map(|m| m.document.clone())
                        .collect();
                    let new = |m: &ConsoleMessage| {
                        m.timestamp >= at && m.document.as_ref().is_none_or(|d| !old.contains(d))
                    };
                    self.messages.retain(new);
                    old
                }
                None => {
                    self.messages.clear();
                    Default::default()
                }
            };
            let cleared = before - self.messages.len();
            if cleared > 0 {
                self.cleared_note = Some(format!(
                    "{cleared} console messages from {previous} were cleared when the tab moved to {site}"
                ));
            }
            self.cleared_before = at;
            // The old site's issues too (ISSUE-60).
            self.issues.reset();
        }
        self.site = Some(site);
    }

    /// The first lines of the `n` newest uncaught errors still in the
    /// buffer, oldest first, each at most `max` characters (see
    /// `get_console_logs::uncaught_headline`).
    pub(crate) fn newest_uncaught(&self, n: usize, max: usize) -> Vec<String> {
        let mut newest: Vec<String> = self
            .messages
            .iter()
            .rev()
            .filter(|m| m.uncaught)
            .take(n)
            .map(|m| get_console_logs::uncaught_headline(m, max))
            .collect();
        newest.reverse();
        newest
    }

    /// The note of the last cross-site clear, once.
    pub(crate) fn take_cleared_note(&mut self) -> Option<String> {
        self.cleared_note.take()
    }

    /// Adds `causes`, one `Caused by:` line each, to the uncaught error
    /// `lookup` was made for, when it is still in the buffer as it was
    /// pushed (a navigation may have cleared it, `clear` removed it, or it
    /// was dropped for a newer one: the causes are then dropped too).
    pub(crate) fn add_causes(&mut self, lookup: &CauseLookup, causes: &[String]) {
        let entry = self.messages.iter_mut().rev().find(|m| {
            m.source == "exception"
                && m.timestamp.to_bits() == lookup.timestamp.to_bits()
                && m.text == lookup.text
        });
        if let Some(entry) = entry {
            let lines: String = causes.iter().map(|c| format!("\n{CAUSED_BY}{c}")).collect();
            entry.text =
                crate::util::text::capped(&format!("{}{lines}", entry.text), CONSOLE_TEXT_MAX);
        }
    }
}

/// How each link of an uncaught error's `cause` chain starts, in its text.
pub(crate) const CAUSED_BY: &str = "Caused by: ";

/// Most links of a `cause` chain read: a chain may loop.
const MAX_CAUSES: usize = 10;

/// How long one cause chain is read for: a busy page answers late, and the
/// error then keeps its text without its causes.
const CAUSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// How long `get_console_logs` waits at most for the cause chains still
/// being read (see [`causes_read`]).
pub(crate) const CAUSES_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// How many cause chains wait to be read: a page throwing in a loop must
/// not queue without bound (the ones over it keep their text as is).
const CAUSE_QUEUE: usize = 64;

/// The `cause` chain of the error the function is called on, as text:
/// each link's stack (an error) or a short preview (any other value), at
/// most `max` links ([`MAX_CAUSES`]), a loop ended by `[Circular]`. It reads `cause`
/// through its property descriptor, so no getter of the page runs for it
/// (a `cause` getter is shown `(...)`, as a preview shows one).
const CAUSE_CHAIN: &str = r#"function (max) {
  const CHARS = 2000;
  const isObject = v => v !== null && (typeof v === 'object' || typeof v === 'function');
  const cut = s => s.length > CHARS ? s.slice(0, CHARS) + '…' : s;
  const cause = o => {
    try { return Object.getOwnPropertyDescriptor(o, 'cause') || null; } catch (_) { return null; }
  };
  const brief = v => typeof v === 'string' ? "'" + (v.length > 100 ? v.slice(0, 100) + '…' : v) + "'"
    : Array.isArray(v) ? 'Array(' + v.length + ')' : isObject(v) ? 'Object' : String(v);
  const text = v => {
    try {
      if (Object.prototype.toString.call(v) === '[object Error]') return String(v.stack || v);
      if (typeof v === 'string') return v;
      if (typeof v === 'function') return 'function ' + (v.name || '(anonymous)');
      if (!isObject(v)) return String(v);
      const array = Array.isArray(v), items = [];
      for (const key of Object.getOwnPropertyNames(v)) {
        const d = Object.getOwnPropertyDescriptor(v, key);
        if (!d || !d.enumerable) continue;
        if (items.length === 5) { items.push('…'); break; }
        const value = 'value' in d ? brief(d.value) : '(...)';
        items.push(array ? value : key + ': ' + value);
      }
      return array ? '[' + items.join(', ') + ']' : '{' + items.join(', ') + '}';
    } catch (_) { return '[unreadable]'; }
  };
  const chain = [], seen = new Set([this]);
  for (let at = this; isObject(at);) {
    const found = cause(at);
    if (!found) break;
    if (!('value' in found)) { chain.push('(...)'); break; }
    if (chain.length === max) { chain.push('… (the chain goes on)'); break; }
    if (seen.has(found.value)) { chain.push('[Circular]'); break; }
    seen.add(found.value);
    chain.push(cut(text(found.value)));
    at = found.value;
  }
  return chain;
}"#;

/// An uncaught error whose `cause` chain is to be read (Chrome): the
/// error's remote object, and how to find its entry in the buffer again.
/// V8's description of an error (its stack) leaves the cause out, and the
/// object cannot be reached once the event is gone, so the chain is read
/// as the error comes, by a task of its own: the listener's ordered stream
/// is never held up.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CauseLookup {
    object_id: String,
    text: String,
    timestamp: f64,
}

/// The lookup of `message`'s cause chain, for an uncaught error Chrome
/// gave a remote object for (`Runtime.exceptionThrown`); none for a text
/// already cut (the causes would be cut away).
fn cause_lookup(method: &str, params: &Value, message: &ConsoleMessage) -> Option<CauseLookup> {
    let exception = params.pointer("/exceptionDetails/exception")?;
    let error =
        method == "Runtime.exceptionThrown" && str_field(exception, "subtype") == Some("error");
    let fits = message.text.chars().count() <= CONSOLE_TEXT_MAX;
    let object_id = str_field(exception, "objectId").filter(|_| error && fits)?;
    Some(CauseLookup {
        object_id: object_id.to_string(),
        text: message.text.clone(),
        timestamp: message.timestamp,
    })
}

/// A lookup queued to the reader, counted in `LogState::causes_pending`
/// as long as it lives: until its causes are in, or until it is dropped
/// unread (the queue was full, or the reader stopped with it queued, as a
/// frame's listeners do when the frame goes: the tab's count, which every
/// `get_console_logs` waits on, stayed up).
struct Queued {
    lookup: CauseLookup,
    _pending: Pending,
}

/// One unit of a count while it lives.
struct Pending(Arc<std::sync::atomic::AtomicUsize>);

impl Pending {
    fn new(count: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Reads the cause chain of each error `lookups` names in the page of
/// `target`, and adds it to its entry (see [`LogState::add_causes`]).
async fn read_causes(
    target: crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
    mut lookups: tokio::sync::mpsc::Receiver<Queued>,
) {
    while let Some(queued) = lookups.recv().await {
        let lookup = &queued.lookup;
        let read = tokio::time::timeout(CAUSE_TIMEOUT, cause_chain(&target, &lookup.object_id));
        let causes = read.await.ok().flatten().unwrap_or_default();
        if !causes.is_empty() {
            state.lock().await.add_causes(lookup, &causes);
        }
        // `queued` goes here: counted out once its causes are in.
    }
}

/// The cause chain of the error `object_id` ([`CAUSE_CHAIN`]); `None` when
/// the page cannot answer (it navigated, it is paused or blocked).
async fn cause_chain(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    object_id: &str,
) -> Option<Vec<String>> {
    let params = serde_json::json!({
        "objectId": object_id,
        "functionDeclaration": CAUSE_CHAIN,
        "arguments": [{"value": MAX_CAUSES}],
        "returnByValue": true,
        "silent": true,
    });
    let reply = target
        .send_raw_command("Runtime.callFunctionOn", params)
        .await
        .ok()?;
    let links = reply.result?.pointer("/result/value")?.as_array()?.clone();
    Some(
        links
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// Waits until no cause chain is being read in `state`, at most `wait`, so
/// a reply right after an uncaught error shows its causes (see
/// [`CauseLookup`]).
pub(crate) async fn causes_read(state: &Arc<Mutex<LogState>>, wait: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    let pending = state.lock().await.causes_pending.clone();
    while pending.load(std::sync::atomic::Ordering::SeqCst) > 0
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// [`process_event`] for an event of a tab or a frame.
#[cfg(test)]
pub(crate) async fn process_log_event(
    event: &WsResponse,
    state: &Arc<Mutex<LogState>>,
) -> Option<CauseLookup> {
    process_event(event, state, &Feed::Page).await
}

/// Applies one event of the ordered stream of `feed`'s session to the
/// tab's console state. Returns the lookup of an uncaught error's cause
/// chain to read, if any (see [`CauseLookup`]).
async fn process_event(
    event: &WsResponse,
    state: &Arc<Mutex<LogState>>,
    feed: &Feed,
) -> Option<CauseLookup> {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return None;
    };
    if let Some(url) = main_frame_commit(method, params) {
        let mut state = state.lock().await;
        state.main_frame = params["frame"]["id"].as_str().map(str::to_string);
        state.on_document_committed(url, None);
    } else if method == "Audits.issueAdded" {
        if let Some(mut issue) = crate::handler::issues::from_chrome(params) {
            // Its node ids are only valid in that session (ISSUE-85).
            issue.session = event.session_id.clone();
            state.lock().await.issues.push(issue);
        }
    } else if method == "Page.navigatedWithinDocument" {
        let mut state = state.lock().await;
        let frame = params["frameId"].as_str();
        let main = state.main_frame.is_none() || state.main_frame.as_deref() == frame;
        if let (true, Some(url)) = (main, params["url"].as_str()) {
            state.note_same_document(url);
        }
    } else if let Some(mut message) = feed.console_message(method, params) {
        message.target_type =
            matches!(feed, Feed::ServiceWorker).then(|| crate::handler::SERVICE_WORKER.to_string());
        let lookup = cause_lookup(method, params, &message);
        let mut state = state.lock().await;
        if !state.is_watched_worker_copy(method, params) {
            state.push_message(message);
            return lookup;
        }
    }
    None
}

/// A console level in the vocabulary both browsers report, lowercase:
/// Firefox's console API names `console.warn` `warn` where Chrome (and
/// Firefox's own warnings) say `warning`, so one `level_filter` works on
/// both.
pub(crate) fn canonical_level(level: &str) -> String {
    let level = level.to_lowercase();
    if level == "warn" {
        "warning".to_string()
    } else {
        level
    }
}

/// The URL of a new top-level document (`Page.frameNavigated` of the main
/// frame, which Chrome sends at commit, before the page's scripts run).
pub(crate) fn main_frame_commit<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    let frame = params
        .get("frame")
        .filter(|_| method == "Page.frameNavigated")?;
    if frame.get("parentId").is_some() {
        return None;
    }
    frame.get("url").and_then(Value::as_str)
}

/// The console message an event describes, if it is one of the three
/// console events.
fn console_message(method: &str, params: &Value) -> Option<ConsoleMessage> {
    match method {
        "Log.entryAdded" => params.get("entry").map(log_entry),
        "Runtime.consoleAPICalled" => Some(console_api_call(params)),
        "Runtime.exceptionThrown" => params
            .get("exceptionDetails")
            .map(|details| exception(params, details)),
        _ => None,
    }
}

fn timestamp(value: &Value) -> f64 {
    value
        .get("timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn log_entry(entry: &Value) -> ConsoleMessage {
    ConsoleMessage {
        source: string_or_empty(entry, "source"),
        level: string_or_empty(entry, "level"),
        text: string_or_empty(entry, "text"),
        timestamp: timestamp(entry),
        url: opt_string(entry, "url"),
        line_number: entry.get("lineNumber").and_then(Value::as_i64),
        channel: Channel::Primary,
        document: None,
        uncaught: false,
        target_type: None,
    }
}

/// Most properties or items a console preview shows.
const PREVIEW_ITEMS: usize = 10;

/// Most characters of a string inside a preview (Chrome cuts its own at 100).
const PREVIEW_STRING_CHARS: usize = 100;

/// `items` between `open` and `close` as the console previews them, with
/// `…` when there are more (`overflow`, or over [`PREVIEW_ITEMS`]).
fn listed(open: &str, close: &str, items: Vec<String>, overflow: bool) -> String {
    let more = overflow || items.len() > PREVIEW_ITEMS;
    let mut shown: Vec<String> = items.into_iter().take(PREVIEW_ITEMS).collect();
    if more {
        shown.push("…".to_string());
    }
    format!("{open}{}{close}", shown.join(", "))
}

/// An object as the console previews it: `{n: 1, s: 'x'}`.
pub(crate) fn object_preview(entries: Vec<(String, String)>, overflow: bool) -> String {
    let items = entries
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    listed("{", "}", items, overflow)
}

/// An array as the console previews it: `[1, 2, 3]`.
pub(crate) fn array_preview(items: Vec<String>, overflow: bool) -> String {
    listed("[", "]", items, overflow)
}

/// A string inside a preview: quoted, and cut at [`PREVIEW_STRING_CHARS`].
pub(crate) fn quoted(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_STRING_CHARS) {
        Some((cut, _)) => format!("'{}…'", &text[..cut]),
        None => format!("'{text}'"),
    }
}

/// A CDP object preview (`RemoteObject.preview`) as text: plain objects
/// and arrays, the rest (errors, maps, nodes...) by their description.
fn preview_text(preview: &Value) -> Option<String> {
    let properties = preview.get("properties")?.as_array()?;
    let overflow = preview.get("overflow").and_then(Value::as_bool) == Some(true);
    let value = |property: &Value| {
        let text = str_field(property, "value").unwrap_or_default();
        match str_field(property, "type") {
            Some("string") => quoted(text),
            Some("undefined") => "undefined".to_string(),
            _ => text.to_string(),
        }
    };
    let description = str_field(preview, "description").unwrap_or_default();
    match str_field(preview, "subtype") {
        Some("array") => Some(array_preview(
            properties.iter().map(value).collect(),
            overflow,
        )),
        None if str_field(preview, "type") == Some("object") => {
            let entries = properties
                .iter()
                .map(|p| {
                    (
                        str_field(p, "name").unwrap_or_default().to_string(),
                        value(p),
                    )
                })
                .collect();
            let object = object_preview(entries, overflow);
            Some(match description {
                "Object" | "" => object,
                class => format!("{class} {object}"),
            })
        }
        _ => None,
    }
}

fn console_api_call(params: &Value) -> ConsoleMessage {
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(arg_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    // Where the call was made; evaluated code has no URL.
    let frame = params
        .pointer("/stackTrace/callFrames/0")
        .filter(|frame| str_field(frame, "url").is_some_and(|url| !url.is_empty()));
    ConsoleMessage {
        source: "console-api".to_string(),
        level: string_or_empty(params, "type"),
        text,
        timestamp: timestamp(params),
        url: frame.and_then(|f| opt_string(f, "url")),
        line_number: frame
            .and_then(|f| f.get("lineNumber"))
            .and_then(Value::as_i64),
        channel: Channel::Primary,
        document: None,
        uncaught: false,
        target_type: None,
    }
}

/// One `console.*` argument as text: strings as is, numbers and booleans
/// printed, objects and arrays previewed, the rest by their description.
fn arg_text(arg: &Value) -> Option<String> {
    if let Some(text) = arg.get("preview").and_then(preview_text) {
        return Some(text);
    }
    if str_field(arg, "type") == Some("undefined") {
        return Some("undefined".to_string());
    }
    let description = str_field(arg, "description").map(str::to_string);
    match arg.get("value") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(val) if val.is_number() || val.is_boolean() => Some(val.to_string()),
        Some(val) => Some(description.unwrap_or_else(|| val.to_string())),
        None => description,
    }
}

/// How an unhandled promise rejection's text starts, on both browsers
/// (Chrome's `exceptionDetails.text`; Firefox's watcher flags it).
pub(crate) const IN_PROMISE: &str = "Uncaught (in promise) ";

/// What two copies of one Firefox message share: its first line without
/// [`IN_PROMISE`]. Firefox reports an uncaught error twice, only its
/// watcher knowing it is a rejection and giving its stack (#204).
fn twin_key(text: &str) -> &str {
    let text = text.strip_prefix(IN_PROMISE).unwrap_or(text);
    text.split('\n').next().unwrap_or(text)
}

/// An uncaught exception's message. Its text is the thrown value as the
/// console prints any value ([`arg_text`]): an error by its description
/// (its stack), a string or a number as it is, an object previewed (#219:
/// only a description was read, so `throw 'x'` was a bare `Uncaught`).
/// Without a thrown value, Chrome's own text.
fn exception(params: &Value, details: &Value) -> ConsoleMessage {
    let thrown = details.get("exception").and_then(arg_text);
    let rejection =
        str_field(details, "text").is_some_and(|t| t.starts_with(IN_PROMISE.trim_end()));
    let text = match thrown {
        Some(thrown) if rejection && !thrown.starts_with("Uncaught") => {
            format!("{IN_PROMISE}{thrown}")
        }
        Some(thrown) => thrown,
        None => string_or_empty(details, "text"),
    };
    ConsoleMessage {
        source: "exception".to_string(),
        level: "error".to_string(),
        text,
        timestamp: timestamp(params),
        url: opt_string(details, "url"),
        line_number: details.get("lineNumber").and_then(Value::as_i64),
        channel: Channel::Primary,
        document: None,
        uncaught: true,
        target_type: None,
    }
}

/// What the session a console listener reads belongs to.
#[derive(Clone, Default)]
pub(crate) enum Feed {
    /// A tab or a frame of another origin.
    #[default]
    Page,
    /// A dedicated worker; the flag is set once it started a worker of
    /// its own, whose uncaught errors it then relays (see [`is_relayed`]).
    Worker(Arc<std::sync::atomic::AtomicBool>),
    /// The tab's service worker: its messages say so (#211).
    ServiceWorker,
}

impl Feed {
    /// The console message the event (`method`, `params`) of this feed
    /// describes, unless the worker that threw it lists it.
    fn console_message(&self, method: &str, params: &Value) -> Option<ConsoleMessage> {
        console_message(method, params).filter(|_| !self.relays(method, params))
    }

    /// Whether the event (`method`, `params`) of this feed is a copy of
    /// what another session reports.
    fn relays(&self, method: &str, params: &Value) -> bool {
        match self {
            Self::Worker(parent) => {
                parent.load(std::sync::atomic::Ordering::Relaxed) && is_relayed(method, params)
            }
            _ => false,
        }
    }
}

/// Whether the event (`method`, `params`) of a worker's session reports an
/// error one of the worker's own workers threw (#217). Chrome fires the
/// error at the `Worker` object of the worker that started it, where
/// nothing handles it, and reports it there too, at the script that threw:
/// an exception with no thrown value (none, or `null`: the value stays in
/// the thread that threw), in each worker up the chain. The worker that
/// threw reports it itself, with its value. Told by the missing value
/// alone: the copy's script may be one the worker imported (#207), and
/// nothing else names the worker. So the `throw null` of a worker that
/// started workers reads the same, and is not listed either. Chrome relays
/// no unhandled rejection: one (`Uncaught (in promise)`) is the worker's
/// own, also with the reason `null` (#223).
pub(crate) fn is_relayed(method: &str, params: &Value) -> bool {
    let details = params.get("exceptionDetails");
    let thrown = details.and_then(|d| d.get("exception"));
    let rejection = details
        .and_then(|d| str_field(d, "text"))
        .is_some_and(|text| text.starts_with("Uncaught (in promise)"));
    method == "Runtime.exceptionThrown"
        && !rejection
        && thrown.is_none_or(|value| str_field(value, "subtype") == Some("null"))
}

/// Feeds `state` from the console of `target`: a tab or a frame of another
/// origin.
pub(crate) fn start_log_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
) -> crate::handler::cdp_domains::event_pump::ListenerHandles {
    start_listener(target, state, Feed::Page)
}

/// Feeds `state` from the console of `target`, as `feed` says: a dedicated
/// worker, or the tab's service worker.
pub(crate) fn start_worker_log_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
    feed: Feed,
) -> crate::handler::cdp_domains::event_pump::ListenerHandles {
    start_listener(target, state, feed)
}

/// Feeds `state` from the console of `target`, the session of `feed`.
fn start_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<LogState>>,
    feed: Feed,
) -> crate::handler::cdp_domains::event_pump::ListenerHandles {
    use crate::handler::cdp_domains::event_pump::{ListenerHandles, spawn_state_listener};
    // One ordered stream of every domain (filtered in `process_event`):
    // a navigation must clear the buffer before the new page's first
    // messages, which separate per-domain tasks would not guarantee.
    let mut handles = ListenerHandles::default();
    let (lookups, queued) = tokio::sync::mpsc::channel(CAUSE_QUEUE);
    handles.push(tokio::spawn(read_causes(
        target.clone(),
        state.clone(),
        queued,
    )));
    handles.push(spawn_state_listener(
        target,
        "",
        state,
        move |event, state| {
            let (lookups, feed) = (lookups.clone(), feed.clone());
            async move {
                let Some(lookup) = process_event(&event, &state, &feed).await else {
                    return;
                };
                let count = state.lock().await.causes_pending.clone();
                // Counted before it is sent; a full queue drops it uncounted.
                let _ = lookups.try_send(Queued {
                    lookup,
                    _pending: Pending::new(&count),
                });
            }
        },
    ));
    handles
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `Runtime.exceptionThrown` of an error `text` with remote object
    /// `object_id`.
    fn thrown(text: &str, object_id: Option<&str>, subtype: &str) -> Value {
        let mut exception = json!({"type": "object", "subtype": subtype, "description": text});
        if let Some(id) = object_id {
            exception["objectId"] = json!(id);
        }
        json!({"timestamp": 7.0, "exceptionDetails": {"text": "Uncaught", "exception": exception}})
    }

    /// #192: the cause chain is read for an uncaught error Chrome gave a
    /// remote object for; not for a thrown value that is not an error, an
    /// error without an object, a console message, nor a text already cut.
    #[test]
    fn the_causes_of_uncaught_errors_are_looked_up() {
        let message = |params: &Value| console_message("Runtime.exceptionThrown", params).unwrap();
        let error = thrown("Error: outer\n    at f (a.js:1:1)", Some("o-1"), "error");
        let lookup = cause_lookup("Runtime.exceptionThrown", &error, &message(&error));
        assert_eq!(
            lookup,
            Some(CauseLookup {
                object_id: "o-1".into(),
                text: "Error: outer\n    at f (a.js:1:1)".into(),
                timestamp: 7.0
            })
        );
        let long = thrown(&"x".repeat(CONSOLE_TEXT_MAX + 1), Some("o-2"), "error");
        for params in [
            thrown("str", Some("o-3"), "string"),
            thrown("Error: e", None, "error"),
            long,
        ] {
            assert_eq!(
                cause_lookup("Runtime.exceptionThrown", &params, &message(&params)),
                None
            );
        }
        let logged = json!({"type": "error", "timestamp": 1.0, "args": [{"type": "object", "subtype": "error", "objectId": "o-4"}]});
        let call = console_message("Runtime.consoleAPICalled", &logged).unwrap();
        assert_eq!(
            cause_lookup("Runtime.consoleAPICalled", &logged, &call),
            None
        );
    }

    /// #192: the causes read are added to the error they were read for,
    /// one `Caused by:` line each, once; an error the buffer no longer has
    /// (a navigation to another site cleared it) gets none, and a second
    /// error with the same text gets its own.
    #[test]
    fn causes_are_added_to_their_own_error() {
        let mut state = LogState::default();
        state.on_document_committed("https://a.test/", None);
        let error = thrown("Error: outer", Some("o-1"), "error");
        for _ in 0..2 {
            state.push_message(console_message("Runtime.exceptionThrown", &error).unwrap());
        }
        let message = console_message("Runtime.exceptionThrown", &error).unwrap();
        let lookup = cause_lookup("Runtime.exceptionThrown", &error, &message).unwrap();
        // When the causes of each come
        state.add_causes(
            &lookup,
            &["TypeError: inner\n    at g".into(), "'why'".into()],
        );
        state.add_causes(&lookup, &["RangeError: other".into()]);
        // Then each error has its own
        let texts: Vec<&str> = state.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Error: outer\nCaused by: RangeError: other",
                "Error: outer\nCaused by: TypeError: inner\n    at g\nCaused by: 'why'"
            ]
        );
        // And causes for an error cleared by a navigation are dropped
        state.on_document_committed("https://b.test/", None);
        state.push_message(console_message("Runtime.exceptionThrown", &error).unwrap());
        state.on_document_committed("https://c.test/", None);
        state.add_causes(&lookup, &["late".into()]);
        assert!(state.messages.iter().all(|m| !m.text.contains("late")));
        // And a long chain is cut with the message
        state.push_message(console_message("Runtime.exceptionThrown", &error).unwrap());
        state.add_causes(&lookup, &["y".repeat(2 * CONSOLE_TEXT_MAX)]);
        let last = &state.messages.back().unwrap().text;
        assert!(
            last.starts_with("Error: outer\nCaused by: yyy") && last.contains("[cut: the first")
        );
    }

    /// #192: the listener reads an uncaught error's cause chain from the
    /// page (`Runtime.callFunctionOn` on the error, by value, silent, with
    /// the chain's cap) without holding up its stream, and the error shows
    /// it; a reader waits for it, bounded.
    #[tokio::test]
    async fn the_listener_reads_the_cause_chain() {
        use crate::handler::cdp_domains::tests::spawn_recording_mock;
        // Given a page whose error has a cause
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.callFunctionOn", |_| {
            json!({"result": {"result": {"type": "object", "value": ["TypeError: inner\n    at g (b.js:2:2)"]}}})
        });
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser =
            cdp_browser_lite::BrowserClient::connect(&address, std::time::Duration::from_secs(2))
                .await
                .unwrap();
        let tab = browser.attach("T-page-1").await.unwrap();
        let target = crate::handler::cdp_domains::cdp_target::CdpTarget::tab(tab.clone());
        let state = Arc::new(Mutex::new(LogState::default()));
        let _listening = start_log_listener(&target, state.clone());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // When the page throws it uncaught
        let error = thrown("Error: outer\n    at f (a.js:1:1)", Some("o-9"), "error");
        mock.emit("Runtime.exceptionThrown", error, Some(tab.session_id()));
        for _ in 0..100 {
            if !state.lock().await.messages.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        causes_read(&state, std::time::Duration::from_secs(2)).await;
        // Then its entry ends with its cause, read as asked
        let text = state.lock().await.messages[0].text.clone();
        assert_eq!(
            text,
            "Error: outer\n    at f (a.js:1:1)\nCaused by: TypeError: inner\n    at g (b.js:2:2)"
        );
        let call = &mock.commands_named("Runtime.callFunctionOn")[0].params;
        assert_eq!(
            (
                &call["objectId"],
                &call["returnByValue"],
                &call["silent"],
                &call["arguments"]
            ),
            (
                &json!("o-9"),
                &json!(true),
                &json!(true),
                &json!([{"value": MAX_CAUSES}])
            )
        );
        assert_eq!(call["functionDeclaration"], CAUSE_CHAIN);
        let pending = state.lock().await.causes_pending.clone();
        assert_eq!(pending.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// #192: a lookup is counted while it lives: a full queue drops it,
    /// and a reader that stops (a frame's listeners end) drops the ones
    /// still queued; either way the count goes back to zero, so
    /// get_console_logs does not wait for them.
    #[tokio::test]
    async fn unread_lookups_are_not_waited_for() {
        let count = LogState::default().causes_pending;
        let lookup = || CauseLookup {
            object_id: "o-1".into(),
            text: "Error: e".into(),
            timestamp: 1.0,
        };
        let queued = || Queued {
            lookup: lookup(),
            _pending: Pending::new(&count),
        };
        let (lookups, reader) = tokio::sync::mpsc::channel(1);
        lookups.try_send(queued()).ok().unwrap();
        assert!(lookups.try_send(queued()).is_err());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(reader);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// #192: a reader waits for the cause chains being read, and no longer
    /// than it was told.
    #[tokio::test]
    async fn readers_wait_for_causes_a_bounded_time() {
        let state = Arc::new(Mutex::new(LogState::default()));
        let count = state.lock().await.causes_pending.clone();
        let pending = Pending::new(&count);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            drop(pending);
        });
        let started = std::time::Instant::now();
        causes_read(&state, std::time::Duration::from_secs(2)).await;
        assert!(started.elapsed() >= std::time::Duration::from_millis(40));
        let _stays = Pending::new(&count);
        let started = std::time::Instant::now();
        causes_read(&state, std::time::Duration::from_millis(60)).await;
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(60)
                && waited < std::time::Duration::from_secs(1)
        );
    }

    /// ISSUE-75 / #207 (Chrome): a watched worker's console comes from its
    /// own session (with its real level); the tab's `Log` copy of it
    /// (`source: "worker"`, `console.log` as `info`), which names the worker
    /// by its target id, is dropped, also for a message of a script the
    /// worker imported (the copy's URL is that script's). Other workers'
    /// copies are kept.
    #[tokio::test]
    async fn watched_workers_are_not_logged_twice() {
        // Given a tab watching the worker of target W1, which imported lib.js
        let state = Arc::new(Mutex::new(LogState::default()));
        state.lock().await.watch_worker("W1");
        let entry = |worker: &str, url: &str| {
            json!({"method": "Log.entryAdded", "params": {"entry": {"source": "worker", "level": "info",
            "text": "obj [object Object]", "url": url, "timestamp": 1.0, "workerId": worker}}})
        };
        // When the tab reports the messages of its script, of lib.js, and of
        // an unwatched worker W2
        let copies = [
            ("W1", "http://a/w.js"),
            ("W1", "http://a/lib.js"),
            ("W2", "http://a/other.js"),
        ];
        for (worker, url) in copies {
            let event: WsResponse = serde_json::from_value(entry(worker, url)).unwrap();
            process_log_event(&event, &state).await;
        }
        // Then only the unwatched worker's is kept
        let urls: Vec<Option<String>> = state
            .lock()
            .await
            .messages
            .iter()
            .map(|m| m.url.clone())
            .collect();
        assert_eq!(urls, [Some("http://a/other.js".to_string())]);
        // And once it stops, its messages are kept again
        state.lock().await.forget_worker("W1");
        let event: WsResponse = serde_json::from_value(entry("W1", "http://a/lib.js")).unwrap();
        process_log_event(&event, &state).await;
        assert_eq!(state.lock().await.messages.len(), 2);
    }

    /// ISSUE-19: console arguments print as the console shows them (object
    /// and array previews, not "Object"), and the call's location is kept
    /// when the code has a URL (evaluated code has none).
    #[test]
    fn console_api_arguments_are_previewed() {
        let events: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/runtime-console-api-called.json"
        ))
        .unwrap();
        let first = console_message("Runtime.consoleAPICalled", &events[0]["params"]).unwrap();
        assert_eq!(
            first.text,
            "hello from page {n: 1, s: 'x', nested: Object} [1, 2, 3] 42 null"
        );
        assert_eq!((first.url, first.line_number), (None, None));
        let error = console_message("Runtime.consoleAPICalled", &events[2]["params"]).unwrap();
        assert!(
            error.text.starts_with("Error: boom-error"),
            "{}",
            error.text
        );
        // A call from a page script keeps its location
        let mut from_page = events[1]["params"].clone();
        from_page["stackTrace"]["callFrames"][0]["url"] = json!("http://a/app.js");
        from_page["stackTrace"]["callFrames"][0]["lineNumber"] = json!(6);
        let warn = console_message("Runtime.consoleAPICalled", &from_page).unwrap();
        assert_eq!(
            (warn.url.as_deref(), warn.line_number),
            (Some("http://a/app.js"), Some(6))
        );
    }

    /// AUD-OB-13: a huge console message is kept cut, on both feeds.
    #[test]
    fn long_console_text_is_capped() {
        let params = json!({"entry": {"source": "javascript", "level": "info", "text": "x".repeat(3_000_000), "timestamp": 1.0}});
        let mut state = LogState::default();
        state.push_message(console_message("Log.entryAdded", &params).unwrap());
        state.push_deduplicated(console_message("Log.entryAdded", &params).unwrap());
        for message in &state.messages {
            assert!(message.text.starts_with(&"x".repeat(CONSOLE_TEXT_MAX)));
            assert!(
                message
                    .text
                    .ends_with("[cut: the first 10000 of 3000000 characters]")
            );
        }
    }

    #[test]
    fn log_entry_becomes_a_console_message() {
        let params = json!({"entry": {"source": "network", "level": "error", "text": "404",
            "timestamp": 1.5, "url": "https://a/x", "lineNumber": 3}});
        let m = console_message("Log.entryAdded", &params).unwrap();
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.text.as_str()),
            ("network", "error", "404")
        );
        assert_eq!(
            (m.timestamp, m.url.as_deref(), m.line_number),
            (1.5, Some("https://a/x"), Some(3))
        );
    }

    #[test]
    fn console_api_arguments_are_joined() {
        let params = json!({"type": "log", "timestamp": 2.0, "args": [
            {"type": "string", "value": "hi"}, {"type": "number", "value": 3},
            {"type": "object", "value": {"a": 1}, "description": "Object"},
            {"type": "object", "description": "Window"}, {"type": "undefined"}]});
        let m = console_message("Runtime.consoleAPICalled", &params).unwrap();
        assert_eq!(m.text, "hi 3 Object Window undefined");
        assert_eq!(
            (m.source.as_str(), m.level.as_str()),
            ("console-api", "log")
        );
    }

    #[test]
    fn exception_prefers_the_exception_description() {
        let params = json!({"timestamp": 4.0, "exceptionDetails": {"text": "Uncaught",
            "url": "https://a/app.js", "lineNumber": 7,
            "exception": {"description": "TypeError: x is undefined"}}});
        let m = console_message("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(m.text, "TypeError: x is undefined");
        assert_eq!(
            (m.source.as_str(), m.level.as_str(), m.line_number),
            ("exception", "error", Some(7))
        );
        let bare = json!({"exceptionDetails": {"text": "Uncaught"}});
        assert_eq!(
            console_message("Runtime.exceptionThrown", &bare)
                .unwrap()
                .text,
            "Uncaught"
        );
        assert!(console_message("Runtime.executionContextCreated", &bare).is_none());
    }

    /// ISSUE-60 (Chrome): the tab keeps the issues Chrome reports
    /// (`Audits.issueAdded`, folded), and forgets them on another site, as
    /// its console messages.
    #[tokio::test]
    async fn issues_are_kept_until_another_site() {
        let state = Arc::new(Mutex::new(LogState::default()));
        let send = |method: &str, params: Value| WsResponse {
            method: Some(method.into()),
            params: Some(params),
            ..Default::default()
        };
        let cors = json!({"issue": {"code": "CorsIssue", "details": {"corsIssueDetails": {"corsErrorStatus": {"corsError": "MissingAllowOriginHeader"}, "request": {"url": "http://b/api"}}}}});
        let page = |url: &str| json!({"frame": {"id": "F", "url": url}});
        process_log_event(&send("Page.frameNavigated", page("http://a/")), &state).await;
        for _ in 0..2 {
            process_log_event(&send("Audits.issueAdded", cors.clone()), &state).await;
        }
        // (the same details twice are one report sent again, ISSUE-85)
        let kept = state.lock().await.issues.list();
        assert_eq!((kept.len(), kept[0].count), (1, 1));
        process_log_event(&send("Page.frameNavigated", page("http://a/other")), &state).await;
        assert_eq!(state.lock().await.issues.list().len(), 1, "same site");
        process_log_event(&send("Page.frameNavigated", page("http://c.test/")), &state).await;
        assert!(state.lock().await.issues.list().is_empty());
    }

    /// ISSUE-81 (Chrome): the main frame's same-document navigations
    /// (history API, fragment) are counted with the URL reached; an
    /// iframe's are not.
    #[tokio::test]
    async fn same_document_navigations_are_counted() {
        let state = Arc::new(Mutex::new(LogState::default()));
        let events = [
            (
                "Page.frameNavigated",
                json!({"frame": {"id": "F", "url": "http://a/"}}),
            ),
            (
                "Page.navigatedWithinDocument",
                json!({"frameId": "F", "url": "http://a/pushed", "navigationType": "historyApi"}),
            ),
            (
                "Page.navigatedWithinDocument",
                json!({"frameId": "C", "url": "http://a/ad#x", "navigationType": "fragment"}),
            ),
        ];
        for (method, params) in events {
            let event = WsResponse {
                method: Some(method.into()),
                params: Some(params),
                ..Default::default()
            };
            process_log_event(&event, &state).await;
        }
        let st = state.lock().await;
        assert_eq!(
            (st.same_document, st.same_document_url.as_deref()),
            (1, Some("http://a/pushed"))
        );
    }

    /// ISSUE-80 (Chrome): an unhandled rejection says `(in promise)`
    /// (Chrome's `text`), not only `Uncaught`.
    #[test]
    fn rejections_say_in_promise() {
        let params = json!({"exceptionDetails": {"text": "Uncaught (in promise)",
            "exception": {"description": "Error: rej\n    at x (http://a/:1:1)"}}});
        let m = console_message("Runtime.exceptionThrown", &params).unwrap();
        assert_eq!(
            m.text,
            "Uncaught (in promise) Error: rej\n    at x (http://a/:1:1)"
        );
    }

    fn message(text: &str) -> ConsoleMessage {
        ConsoleMessage {
            source: "console-api".into(),
            level: "log".into(),
            text: text.into(),
            timestamp: 0.0,
            url: None,
            line_number: None,
            channel: Channel::Primary,
            document: None,
            uncaught: false,
            target_type: None,
        }
    }

    fn event(method: &str, params: Value) -> WsResponse {
        WsResponse {
            method: Some(method.into()),
            params: Some(params),
            ..Default::default()
        }
    }

    fn committed(url: &str) -> WsResponse {
        event(
            "Page.frameNavigated",
            serde_json::json!({"frame": {"id": "F", "url": url}, "type": "Navigation"}),
        )
    }

    async fn texts(state: &Arc<Mutex<LogState>>) -> Vec<String> {
        let st = state.lock().await;
        st.messages.iter().map(|m| m.text.clone()).collect()
    }

    /// CON-01 / CON-02 / CON-04 (Chrome): a cross-site main-frame commit
    /// clears the buffer; subdomains of the site and reloads keep it.
    #[tokio::test]
    async fn cross_site_navigation_clears_the_console() {
        // Given a tab on www.knot.kz with one message
        let state = Arc::new(Mutex::new(LogState::default()));
        process_log_event(&committed("https://www.knot.kz/"), &state).await;
        state.lock().await.push_message(message("home"));
        // When it moves to docs.knot.kz and reloads, the message stays
        process_log_event(&committed("https://docs.knot.kz/a"), &state).await;
        process_log_event(&committed("https://docs.knot.kz/a"), &state).await;
        assert_eq!(texts(&state).await, ["home"]);
        // And an iframe navigating elsewhere changes nothing
        let iframe = event(
            "Page.frameNavigated",
            serde_json::json!({"frame": {"id": "C", "parentId": "F", "url": "https://ads.example/"}}),
        );
        process_log_event(&iframe, &state).await;
        assert_eq!(texts(&state).await, ["home"]);
        // When the tab moves to another site, the buffer is cleared, once noted
        process_log_event(&committed("https://example.com/"), &state).await;
        assert!(texts(&state).await.is_empty());
        let note = state.lock().await.take_cleared_note().unwrap();
        assert_eq!(
            note,
            "1 console messages from knot.kz were cleared when the tab moved to example.com"
        );
        assert!(state.lock().await.take_cleared_note().is_none());
    }

    /// FWS-04: a message both Firefox feeds report is kept once (the
    /// watcher's copy), the two copies stamped the same millisecond (BiDi
    /// without the watcher's fraction); repeats within one feed are all
    /// kept, and so are copies of the same text stamped apart: a page's
    /// error repeated 300 ms later, its four copies arriving out of order,
    /// is two errors, each with its own copy and stamp.
    #[test]
    fn duplicates_across_feeds_are_kept_once() {
        let at = |text: &str, ms: f64, channel: Channel| ConsoleMessage {
            timestamp: ms,
            channel,
            source: if channel == Channel::Watcher {
                "security"
            } else {
                "console-api"
            }
            .into(),
            ..message(text)
        };
        let mut state = LogState::default();
        // Watcher first, then BiDi's copy: the copy is dropped
        state.push_deduplicated(at("cookie rejected", 1000.4, Channel::Watcher));
        state.push_deduplicated(at("cookie rejected", 1000.0, Channel::Primary));
        // BiDi first, then the watcher's copy: replaced in place
        state.push_deduplicated(at("boom", 2000.0, Channel::Primary));
        state.push_deduplicated(at("boom", 2000.7, Channel::Watcher));
        // The same text twice from one feed: both kept
        state.push_deduplicated(at("tick", 3000.0, Channel::Primary));
        state.push_deduplicated(at("tick", 3001.0, Channel::Primary));
        // An error and its repeat 300 ms later: both BiDi copies first
        state.push_deduplicated(at("again", 4000.0, Channel::Primary));
        state.push_deduplicated(at("again", 4300.0, Channel::Primary));
        state.push_deduplicated(at("again", 4000.2, Channel::Watcher));
        state.push_deduplicated(at("again", 4300.9, Channel::Watcher));
        let kept: Vec<(&str, &str, f64)> = state
            .messages
            .iter()
            .map(|m| (m.text.as_str(), m.source.as_str(), m.timestamp))
            .collect();
        assert_eq!(
            kept,
            [
                ("cookie rejected", "security", 1000.4),
                ("boom", "security", 2000.7),
                ("tick", "console-api", 3000.0),
                ("tick", "console-api", 3001.0),
                ("again", "security", 4000.2),
                ("again", "security", 4300.9),
            ]
        );
    }

    /// A copy of the uncaught error `text` from `channel`, stamped `ms`.
    fn copy(text: &str, ms: f64, channel: Channel) -> ConsoleMessage {
        ConsoleMessage {
            timestamp: ms,
            channel,
            uncaught: true,
            source: "exception".into(),
            ..message(text)
        }
    }

    /// The stamps of the buffer's messages, in its order.
    fn stamps(state: &LogState) -> Vec<f64> {
        state.messages.iter().map(|m| m.timestamp).collect()
    }

    /// #218 (Firefox): of errors with the same text thrown in one
    /// millisecond, each watcher copy takes the oldest BiDi copy still
    /// waiting, so they stay oldest first, each once and counted once; a
    /// neighbour stamped in the next millisecond is never taken instead.
    #[test]
    fn a_burst_of_one_error_is_paired_oldest_first() {
        let mut state = LogState::default();
        // Given BiDi's copies of five throws, three of them in one millisecond
        for ms in [992.0, 992.0, 992.0, 993.0, 995.0] {
            state.push_deduplicated(copy("Error: burst\n    at f", ms, Channel::Primary));
        }
        // When the watcher's copies come, in the order they were thrown
        for ms in [992.522, 992.533, 992.9, 993.1, 995.7] {
            state.push_deduplicated(copy("Error: burst\n    at g", ms, Channel::Watcher));
        }
        // Then each throw is listed once, as the watcher gave it, in order
        assert_eq!(stamps(&state), [992.522, 992.533, 992.9, 993.1, 995.7]);
        assert!(state.messages.iter().all(|m| m.text.ends_with("at g")));
        assert_eq!(state.uncaught, 5);
        // And a third copy of a paired throw is another throw
        state.push_deduplicated(copy("Error: burst", 995.0, Channel::Primary));
        assert_eq!(state.messages.len(), 6);
    }

    /// #218 (Firefox): a copy finds its twin however many messages came
    /// since (BiDi can run 100 messages ahead of the watcher: a lookback
    /// of 64 listed those errors twice).
    #[test]
    fn a_copy_far_behind_finds_its_twin() {
        let mut state = LogState::default();
        for throw in 0..300_u32 {
            let ms = 1000.0 + f64::from(throw) * 3.0;
            state.push_deduplicated(copy("Error: burst", ms, Channel::Primary));
        }
        for throw in 0..300_u32 {
            let ms = 1000.4 + f64::from(throw) * 3.0;
            state.push_deduplicated(copy("Error: burst", ms, Channel::Watcher));
        }
        assert_eq!((state.messages.len(), state.uncaught), (300, 300));
        assert!(state.messages.iter().all(|m| m.channel == Channel::Both));
        let listed = stamps(&state);
        assert!(listed.is_sorted(), "{listed:?}");
    }

    /// The buffer is read oldest first whatever order the feeds deliver
    /// in: a message is put where its stamp says, after those stamped the
    /// same; one without a stamp stays where it arrived.
    #[test]
    fn messages_are_kept_in_the_order_of_their_stamps() {
        let at = |text: &str, ms: f64| ConsoleMessage {
            timestamp: ms,
            ..message(text)
        };
        let mut state = LogState::default();
        // Given a worker's messages arriving before the page's older ones
        for (text, ms) in [("worker 2", 20.5), ("page 1", 10.0), ("worker 3", 30.0)] {
            state.push_message(at(text, ms));
        }
        state.push_message(at("page 2", 20.5));
        // And one without a stamp, then an older stamped one
        state.push_message(message("unstamped"));
        state.push_message(at("late", 5.0));
        let texts: Vec<&str> = state.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "page 1",
                "worker 2",
                "page 2",
                "worker 3",
                "unstamped",
                "late"
            ]
        );
    }

    /// #219 (Chrome): an uncaught value that is not an error is printed as
    /// the console prints it, a rejection after `(in promise)`; an error
    /// keeps its description.
    #[test]
    fn thrown_values_are_printed() {
        let thrown = |text: &str, exception: Value| {
            let details = json!({"text": text, "exception": exception});
            let params = json!({"timestamp": 1.0, "exceptionDetails": details});
            console_message("Runtime.exceptionThrown", &params)
                .unwrap()
                .text
        };
        let object = json!({"type": "object", "className": "Object", "description": "Object",
            "preview": {"type": "object", "description": "Object", "overflow": false,
                "properties": [{"name": "a", "type": "number", "value": "1"}]}});
        let cases = [
            (json!({"type": "string", "value": "str-boom"}), "str-boom"),
            (
                json!({"type": "number", "value": 42, "description": "42"}),
                "42",
            ),
            (json!({"type": "boolean", "value": true}), "true"),
            (
                json!({"type": "object", "subtype": "null", "value": null}),
                "null",
            ),
            (json!({"type": "undefined"}), "undefined"),
            (
                json!({"type": "bigint", "unserializableValue": "10n", "description": "10n"}),
                "10n",
            ),
            (
                json!({"type": "symbol", "description": "Symbol(s)"}),
                "Symbol(s)",
            ),
            (object, "{a: 1}"),
        ];
        for (exception, text) in cases {
            assert_eq!(thrown("Uncaught", exception), text);
        }
        let rejected = json!({"type": "string", "value": "rej-str"});
        assert_eq!(
            thrown("Uncaught (in promise)", rejected),
            "Uncaught (in promise) rej-str"
        );
        let error = json!({"type": "object", "subtype": "error", "description": "Error: boom\n    at a.js:1:7",
            "preview": {"type": "object", "subtype": "error", "description": "Error: boom", "overflow": false, "properties": []}});
        assert_eq!(thrown("Uncaught", error), "Error: boom\n    at a.js:1:7");
    }

    /// #217 (Chrome): the error a worker relays for one of its workers
    /// has no thrown value (none, or `null`); one it threw itself has one,
    /// and so has a rejection. Only a worker that started a worker relays.
    #[test]
    fn relayed_errors_have_no_thrown_value() {
        let thrown = |exception: Option<Value>| {
            let mut details =
                json!({"text": "Uncaught", "url": "http://a/inner.js", "lineNumber": 0});
            if let Some(exception) = exception {
                details["exception"] = exception;
            }
            json!({"timestamp": 1.0, "exceptionDetails": details})
        };
        let null = thrown(Some(
            json!({"type": "object", "subtype": "null", "value": null}),
        ));
        let none = thrown(None);
        let own = thrown(Some(
            json!({"type": "object", "subtype": "error", "description": "Error: boom"}),
        ));
        let string = thrown(Some(json!({"type": "string", "value": "str"})));
        let relayed = |params: &Value| is_relayed("Runtime.exceptionThrown", params);
        assert!(relayed(&null) && relayed(&none));
        assert!(!relayed(&own) && !relayed(&string));
        assert!(!is_relayed("Runtime.consoleAPICalled", &none));
        // A worker's own `throw null` is kept until it starts a worker
        let parent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Feed::Worker(parent.clone());
        assert!(!worker.relays("Runtime.exceptionThrown", &null));
        parent.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(worker.relays("Runtime.exceptionThrown", &null));
        assert!(!worker.relays("Runtime.exceptionThrown", &own));
        // And a page relays nothing
        assert!(!Feed::Page.relays("Runtime.exceptionThrown", &null));
    }

    /// #223 (Chrome): a worker relays its workers' uncaught errors, never
    /// their unhandled rejections, so its own `Promise.reject(null)` (an
    /// `Uncaught (in promise)` with a null value) is its own and listed.
    #[test]
    fn a_rejection_with_reason_null_is_never_a_relay() {
        // Given a worker that started a worker of its own
        let worker = Feed::Worker(Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let reason = json!({"type": "object", "subtype": "null", "value": null});
        let details = |text: &str| {
            json!({"timestamp": 1.0, "exceptionDetails": {"text": text,
                "url": "http://a/outer.js", "lineNumber": 1, "exception": reason.clone()}})
        };
        // When it rejects a promise with null, and when it relays a throw
        let rejected = details("Uncaught (in promise)");
        let relayed = details("Uncaught");
        // Then the rejection is listed, and the relay is not
        assert!(!worker.relays("Runtime.exceptionThrown", &rejected));
        assert!(worker.relays("Runtime.exceptionThrown", &relayed));
        let listed = worker.console_message("Runtime.exceptionThrown", &rejected);
        assert_eq!(
            listed.map(|m| m.text),
            Some("Uncaught (in promise) null".to_string())
        );
    }

    /// REV-F11-5: with the commit time, messages the new page logged before
    /// the commit event arrived stay, and the old site's late ones go.
    #[test]
    fn commit_time_keeps_early_new_page_messages() {
        let at = |text: &str, ms: f64| ConsoleMessage {
            timestamp: ms,
            ..message(text)
        };
        let mut state = LogState::default();
        state.on_document_committed("https://a.test/", Some(0.0));
        state.push_deduplicated(at("old", 1000.0));
        state.push_deduplicated(at("new early", 2500.0));
        // When b.test commits at 2000
        state.on_document_committed("https://b.test/", Some(2000.0));
        // And a late message of a.test arrives
        state.push_deduplicated(at("old late", 1900.0));
        state.push_deduplicated(at("new", 2600.0));
        let texts: Vec<&str> = state.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["new early", "new"]);
        assert_eq!(
            state.take_cleared_note().unwrap(),
            "1 console messages from a.test were cleared when the tab moved to b.test"
        );
    }

    /// #210: each uncaught error is counted once, whichever Firefox feed
    /// reports it first, also when only one copy says it is one; console
    /// messages are not counted, and a navigation or `clear` empties the
    /// buffer without lowering the count. The newest still logged are
    /// quoted by their first line, as get_console_logs begins them.
    #[test]
    fn uncaught_errors_are_counted_once() {
        let copy = |text: &str, ms: f64, channel: Channel, uncaught: bool| ConsoleMessage {
            timestamp: ms,
            channel,
            uncaught,
            source: "exception".into(),
            ..message(text)
        };
        let mut state = LogState::default();
        state.on_document_committed("https://a.test/", None);
        // BiDi first, then the watcher; the watcher first, then BiDi
        state.push_deduplicated(copy("Error: one\n    at f", 10.0, Channel::Primary, true));
        state.push_deduplicated(copy("Error: one\n    at f", 10.3, Channel::Watcher, true));
        state.push_deduplicated(copy(
            "uncaught exception: two",
            20.6,
            Channel::Watcher,
            true,
        ));
        state.push_deduplicated(copy(
            "uncaught exception: two",
            20.0,
            Channel::Primary,
            true,
        ));
        // A twin only one copy flags, and a console message
        state.push_deduplicated(copy("SyntaxError: three", 30.0, Channel::Watcher, false));
        state.push_deduplicated(copy("SyntaxError: three", 30.1, Channel::Primary, true));
        state.push_deduplicated(message("logged"));
        assert_eq!(state.uncaught, 3);
        assert_eq!(
            state.newest_uncaught(2, 200),
            ["uncaught exception: two", "Uncaught SyntaxError: three"]
        );
        // When another site clears the buffer, the count stays
        state.on_document_committed("https://b.test/", None);
        assert_eq!(state.uncaught, 3);
        assert!(state.newest_uncaught(3, 200).is_empty());
    }

    /// #206 (Firefox): the new page's first messages, stamped after its
    /// navigation started but before Firefox stamped the commit, stay; the
    /// old document's go, also when they are stamped after the start or
    /// arrive late.
    #[test]
    fn navigation_start_keeps_the_new_page_first_messages() {
        let at = |text: &str, ms: f64, document: Option<&str>| ConsoleMessage {
            timestamp: ms,
            document: document.map(str::to_string),
            ..message(text)
        };
        // Given a tab on a.test whose document "ra" logged, and a
        // navigation to b.test started at 2000
        let mut state = LogState::default();
        state.on_navigation_committed("https://a.test/", None, Some(0.0));
        state.push_deduplicated(at("old", 1000.0, Some("ra")));
        state.on_navigation_started("n2", 2000.0);
        state.push_deduplicated(at("old unloading", 2050.0, Some("ra")));
        state.push_deduplicated(at("new before the commit", 2100.0, Some("rb")));
        // When b.test commits, stamped 2300
        state.on_navigation_committed("https://b.test/", Some("n2"), Some(2300.0));
        // And messages arrive late: the new page's, stamped before the
        // commit, the old document's, and one of no known document
        state.push_deduplicated(at("new late", 2200.0, Some("rb")));
        state.push_deduplicated(at("old late", 2250.0, Some("ra")));
        state.push_deduplicated(at("unknown before the start", 1900.0, None));
        state.push_deduplicated(at("unknown after the start", 2400.0, None));
        // Then only the new page's and the later unknown one are kept
        let texts: Vec<&str> = state.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "new before the commit",
                "new late",
                "unknown after the start"
            ]
        );
        assert_eq!(
            state.take_cleared_note().unwrap(),
            "2 console messages from a.test were cleared when the tab moved to b.test"
        );
        // And a commit of another navigation than the one started counts
        // from its own time
        state.on_navigation_started("n3", 3000.0);
        state.on_navigation_committed("https://c.test/", Some("n4"), Some(3500.0));
        assert!(state.messages.is_empty());
    }

    /// CON-03 / CON-07: same-document navigations keep the buffer; sibling
    /// subdomains of a public suffix are different sites.
    #[tokio::test]
    async fn hash_navigation_keeps_and_public_suffix_clears() {
        // Given a tab on a.github.io with one message
        let state = Arc::new(Mutex::new(LogState::default()));
        process_log_event(&committed("https://a.github.io/"), &state).await;
        state.lock().await.push_message(message("a"));
        // When it navigates within the document, nothing is cleared
        let hash = event(
            "Page.navigatedWithinDocument",
            serde_json::json!({"frameId": "F", "url": "https://a.github.io/#x"}),
        );
        process_log_event(&hash, &state).await;
        assert_eq!(texts(&state).await, ["a"]);
        // When it moves to b.github.io, the buffer is cleared
        process_log_event(&committed("https://b.github.io/"), &state).await;
        assert!(texts(&state).await.is_empty());
    }
}
