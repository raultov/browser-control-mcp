use crate::backend::BackendError;
use crate::handler::NetworkState;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::interruption::Interruption;
use crate::handler::navigation::{
    DOWNLOAD, LOAD_TIMEOUT, NOT_LOADED_NOTE, NavigationOutcome, document_response, load_before,
};
use crate::handler::{BrowserSession, McpHandler, is_local_address};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[macros::mcp_tool(
    name = "navigate",
    description = "Loads a URL in the tab, or moves back or forward in its history, and waits for load (at most 30 s; a breakpoint or dialog while loading fails at once saying so). Returns {url (after redirects), tab_id, browser, status?, notes?}. A network error, a download or a page behind HTTP authentication (log in with http://user:password@host/path) fails with 'Navigation failed: <reason> (<url>)'. With --local only local addresses are allowed. Note: copy_cookies imports the user's real Chrome cookies: ask the user first. On a running instance it relaunches it, closing its tabs: the call first fails listing them, then re-issue it with confirm_restart: true."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct NavigateTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Absolute URL (http, https or file); omit it to use 'history'.
    pub url: Option<String>,
    /// 'back' or 'forward' in the tab's history, instead of a 'url'.
    pub history: Option<HistoryStep>,
    /// Import the user's Chrome cookies (needs --allow-cookie-import; Chrome only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_cookies: Option<bool>,
    /// Chrome profile to import from (e.g. 'Profile 1'). Default: the last used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_profile: Option<String>,
    /// Allow relaunching a running instance to import cookies (closes its tabs). Default: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_restart: Option<bool>,
}

/// A move in the tab's history: one entry back or forward.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum HistoryStep {
    /// The previous entry.
    #[serde(rename = "back")]
    Back,
    /// The next entry.
    #[serde(rename = "forward")]
    Forward,
}

impl HistoryStep {
    /// The history offset: -1 back, 1 forward.
    pub(crate) fn delta(self) -> i64 {
        match self {
            Self::Back => -1,
            Self::Forward => 1,
        }
    }

    /// The reply's opening words, before the URL arrived at.
    pub(crate) fn went(self) -> &'static str {
        match self {
            Self::Back => "Went back to",
            Self::Forward => "Went forward to",
        }
    }

    /// The error when there is no entry that way.
    pub(crate) fn missing(self) -> String {
        let page = match self {
            Self::Back => "previous",
            Self::Forward => "next",
        };
        format!("There is no {page} page in the tab's history; nothing moved.")
    }
}

/// Where a `navigate` call goes.
pub(crate) enum Destination<'a> {
    /// A URL to load.
    Url(&'a str),
    /// One entry back or forward in the tab's history.
    History(HistoryStep),
}

impl NavigateTool {
    /// The URL or the history move this call asks for (exactly one).
    pub(crate) fn destination(&self) -> Result<Destination<'_>, CallToolError> {
        match (self.url.as_deref(), self.history) {
            (Some(url), None) => crate::handler::navigation::checked_url(url)
                .map(|()| Destination::Url(url))
                .map_err(CallToolError::from_message),
            (None, Some(step)) => Ok(Destination::History(step)),
            _ => Err(CallToolError::from_message(
                "Pass either 'url' or 'history' ('back' or 'forward'), not both.",
            )),
        }
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: NavigateTool = crate::handler::parse_args(params)?;
        let destination = args.destination()?;
        let session = handler.session(args.instance_id.clone()).await?;

        if let Destination::Url(url) = destination
            && handler.flags.local
            && !is_local_address(url)
        {
            return Err(CallToolError::from_message(format!(
                "Navigation to '{url}' is blocked. This MCP server is running with the 'local' argument, which restricts navigation to local addresses only (localhost, 127.0.0.1, 192.168.x.x, or *.local). To allow navigation to external addresses, restart the MCP server without the 'local' argument."
            )));
        }
        if args.copy_cookies == Some(true) {
            args.import_cookies(handler, &session).await?;
        }
        let target = session.target_to_load(args.tab_id.clone()).await?;
        let network = session.network_state(args.tab_id.clone())?;
        let navigation = async {
            Ok::<_, CallToolError>(match destination {
                Destination::Url(url) => {
                    (load_and_wait(&target, &network, url).await?, "Navigated to")
                }
                Destination::History(step) => (
                    traverse_and_wait(&target, &network, step).await?,
                    step.went(),
                ),
            })
        };
        // A page paused in the debugger (or showing a dialog) while it
        // loads never fires `load`: the call ends saying so, as on Firefox.
        // Bound first: the call's error is not Send, so it must not live
        // across the await of `interrupted`.
        let raced = match target.race_interruptions("navigate", navigation).await {
            Ok(navigated) => Ok(navigated?),
            Err(stopped) => Err(stopped),
        };
        let (outcome, went) = match raced {
            Ok(navigated) => navigated,
            Err(stopped) => {
                let local = handler.flags.local;
                return Err(interrupted(&target, local, &stopped, "navigate").await);
            }
        };
        // A redirect may have taken the tab to a remote address, or a history
        // entry be one a redirect left there. Checked once the race is over:
        // a dialog the page opens while the tab is sent back to about:blank
        // must not end the call before that is done.
        stay_local(&target, handler.flags.local, &outcome.url).await?;
        let tab_id = session.tab_id_of(args.tab_id.as_deref());
        Ok(outcome.reply(&format!("{went} {}.", outcome.url), tab_id.as_deref()))
    }

    /// Seeds the instance's profile with the user's cookies: on the next
    /// launch if it is not running, else by relaunching it (only after the
    /// user confirmed, because every tab is closed).
    async fn import_cookies(
        &self,
        handler: &McpHandler,
        session: &std::sync::Arc<BrowserSession>,
    ) -> Result<(), CallToolError> {
        use crate::handler::chrome_instance::cookie_seed::{
            RealEnvProvider, RealFsProbe, resolve_source, seed_cookies,
        };
        if !handler.flags.allow_cookie_import {
            return Err(CallToolError::from_message(
                "Cookie import is disabled. Restart the MCP server with the '--allow-cookie-import' argument to enable it.",
            ));
        }
        if handler.base_params.browser.user_profile {
            return Err(CallToolError::from_message(
                "Cookie import is redundant when running in --user-profile mode, as the user's real profile is already in use.",
            ));
        }
        let source = resolve_source(
            self.source_profile.as_deref(),
            &RealEnvProvider,
            &RealFsProbe,
        )
        .map_err(CallToolError::from_message)?;
        let is_running = session.chrome_manager.lock().await.is_running().await;
        if is_running && self.confirm_restart != Some(true) {
            let instance_id = self.instance_id.as_deref().unwrap_or("default");
            return Err(CallToolError::from_message(restart_warning(
                instance_id,
                session,
            )));
        }
        let report = seed_cookies(&source, &RealEnvProvider, &RealFsProbe)
            .map_err(CallToolError::from_message)?;
        if is_running {
            crate::handler::chrome_instance::seeded_relaunch::relaunch_seeded(session, report)
                .await
                .map_err(CallToolError::from_message)
        } else {
            session
                .chrome_manager
                .lock()
                .await
                .set_seed_profile(Some(report));
            Ok(())
        }
    }
}

/// Navigates `target` to `url` and waits for its `load` ([`load_and_wait`]).
/// With `local` (the server runs with `--local`), a navigation that ends,
/// after redirects, on an address that is not local fails and the tab is
/// sent back to about:blank ([`crate::handler::navigation::LEFT_LOCAL`]).
pub(crate) async fn navigate_and_wait(
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
    url: &str,
    local: bool,
) -> Result<NavigationOutcome, CallToolError> {
    let outcome = load_and_wait(target, network, url).await?;
    stay_local(target, local, &outcome.url).await?;
    Ok(outcome)
}

/// With `local` (the server runs with `--local`), fails when the tab ended
/// on `landed`, an address that is not local, after sending it back to
/// about:blank ([`crate::handler::navigation::LEFT_LOCAL`], [`blank`]).
pub(crate) async fn stay_local(
    target: &CdpTarget,
    local: bool,
    landed: &str,
) -> Result<(), CallToolError> {
    if !crate::handler::navigation::left_local(local, landed) {
        return Ok(());
    }
    let blanked = blank(target).await;
    Err(crate::handler::navigation::left_local_error(landed, blanked).into())
}

/// The error of `call` (a navigation, a reload), which `stopped` left
/// unfinished. With `local`, a dialog a page that is not local opened while
/// loading (a redirect's target), or a debugger pause it hit, does not keep
/// it: the tab is sent back to about:blank, as when it loads
/// ([`stay_local`]), and the error is the `--local` one, never the dialog's
/// text or the pause, so no later tool reads the page (#117:
/// `handle_dialog` let it finish loading; #124: `resume` did).
pub(crate) async fn interrupted(
    target: &CdpTarget,
    local: bool,
    stopped: &Interruption,
    call: &str,
) -> CallToolError {
    let blocked = matches!(stopped, Interruption::Dialog(_) | Interruption::Paused(_));
    if local
        && blocked
        && let Some(landed) = committed_url(target).await
        && let Err(error) = stay_local(target, local, &landed).await
    {
        return error;
    }
    CallToolError::from_message(stopped.during(call))
}

/// How long [`blank`] waits for about:blank to load (it loads nothing).
const BLANK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How many times [`blank`] tries: a page can open another dialog, or
/// pause again, between the closing of one and the navigation.
const BLANK_TRIES: usize = 3;

/// Sends the tab of `target` to about:blank; whether it got there. Each
/// try first closes what blocks the page ([`unblock`]): an open dialog
/// made the navigation fail, and a pause would hold it. A dialog or a pause
/// the page starts during a try (a burst of alerts, a second `debugger;`)
/// ends that try, and the next one closes it (#123: one failed blanking
/// left the remote page readable).
async fn blank(target: &CdpTarget) -> bool {
    for _ in 0..BLANK_TRIES {
        unblock(target).await;
        if let Ok(Some(blanked)) = target
            .race_interruptions("navigate", load_blank(target))
            .await
        {
            return blanked;
        }
    }
    false
}

/// Closes what may block the page of `target`, whose content no tool may
/// read: dismisses an open dialog, and resumes a pause, ending the script
/// that paused (`terminateOnResume`), on the tab or the frame it is in.
/// Both are sent whatever the tab's state says (its listeners may not have
/// seen the dialog or the pause yet); Chrome refuses the ones with nothing
/// to close, which is fine.
async fn unblock(target: &CdpTarget) {
    let dismiss = json!({"accept": false});
    let _ = target
        .send_raw_command("Page.handleJavaScriptDialog", dismiss)
        .await;
    let resume = json!({"terminateOnResume": true});
    let paused = target.paused_target().await;
    let _ = paused.send_raw_command("Debugger.resume", resume).await;
}

/// Navigates `target` to about:blank and waits (at most [`BLANK_WAIT`])
/// for it to load: Chrome answers `Page.navigate` before that, and a tool
/// called right after met "Not attached to an active page". Whether Chrome
/// took the navigation; `None` when a dialog or a pause of the page
/// interrupted the command.
async fn load_blank(target: &CdpTarget) -> Option<bool> {
    let mut page_events = target.on_domain("Page");
    let blank = json!({"url": "about:blank"});
    let result = match target.send_raw_command("Page.navigate", blank).await {
        Ok(reply) => reply.result,
        Err(error) if crate::handler::dialogs::interrupted(&error) => return None,
        Err(_) => return Some(false),
    };
    // Chrome reports a failed navigation as `errorText` in a success.
    let blanked = result
        .as_ref()
        .is_some_and(|r| r.get("errorText").is_none());
    if let Some(loader) = result.as_ref().and_then(|r| r["loaderId"].as_str()) {
        let loaded = wait_for_load_of(&mut page_events, loader);
        let _ = tokio::time::timeout(BLANK_WAIT, loaded).await;
    }
    Some(blanked)
}

/// `Page.navigate` to `url`, the tab's Network domain enabled first: a
/// fresh tab is registered, and so navigated, while its domains are still
/// being enabled (#133: the first page's requests went unseen). Chrome
/// answers a session's commands in order: once this (idempotent)
/// `Network.enable` replies, the page's requests are recorded.
async fn send_navigation(
    target: &CdpTarget,
    url: &str,
) -> cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse> {
    let _ = target.send_raw_command("Network.enable", json!({})).await;
    target
        .send_raw_command("Page.navigate", json!({"url": url}))
        .await
}

/// Navigates `target` and waits (at most [`LOAD_TIMEOUT`]) for the new
/// document to fire `load`. Chrome answers `Page.navigate` as soon as the
/// navigation starts and reports a network error only as `errorText` inside
/// a successful reply: that becomes a tool error here ([`check_error`]).
/// `network` is the tab's request log, where the document's HTTP status is.
pub(crate) async fn load_and_wait(
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
    url: &str,
) -> Result<NavigationOutcome, CallToolError> {
    // Subscribed first: `load` can fire before the reply is read.
    let mut page_events = target.on_domain("Page");
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    let reply = match send_navigation(target, url).await {
        Ok(reply) => reply,
        // Chrome answers once the response headers arrive: a slow server
        // outlasts the command timeout while the navigation goes on.
        Err(cdp_browser_lite::CdpError::Timeout { .. }) => {
            let loaded = load_before(&mut page_events, deadline, is_load).await;
            let outcome = finish(target, url, json!({}), loaded).await;
            return Ok(with_status(outcome, target, network).await);
        }
        Err(e) => {
            return Err(CallToolError::from_message(crate::handler::cdp_failure(
                "CDP Error",
                &e,
            )));
        }
    };
    let browser = reply.result.unwrap_or_else(|| json!({}));
    check_error(&browser, network, url).await?;
    // Same-document navigations (fragment, history) have no loader and fire
    // no `load`; their URL is the one `navigatedWithinDocument` gives: the
    // history entry may still hold the previous one (ISSUE-70).
    if browser.get("loaderId").is_none() {
        let frame = browser.get("frameId").and_then(Value::as_str);
        if let Some(moved) = moved_within_document(&mut page_events, frame).await {
            let outcome = NavigationOutcome {
                url: moved,
                browser,
                ..Default::default()
            };
            return Ok(with_status(outcome, target, network).await);
        }
        let outcome = finish(target, url, browser, true).await;
        return Ok(with_status(outcome, target, network).await);
    }
    let loader = browser
        .get("loaderId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let loaded = wait_for_load_of(&mut page_events, loader).await;
    let outcome = finish(target, url, browser, loaded).await;
    Ok(with_status(outcome, target, network).await)
}

/// How long a same-document navigation waits for Chrome's
/// `navigatedWithinDocument` (it comes with or right after the reply).
const WITHIN_DOCUMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The URL `Page.navigatedWithinDocument` gives for `frame` (the page's
/// frame, from the `Page.navigate` reply; any frame when unknown), within
/// [`WITHIN_DOCUMENT_WAIT`]; `None` when it does not come.
async fn moved_within_document(
    events: &mut cdp_browser_lite::EventFilter,
    frame: Option<&str>,
) -> Option<String> {
    use tokio_stream::StreamExt;
    let moved = async {
        while let Some(event) = events.next().await {
            let Ok(event) = event else { continue };
            if event.method.as_deref() != Some("Page.navigatedWithinDocument") {
                continue;
            }
            let params = event.params.unwrap_or_default();
            if frame.is_none_or(|f| params["frameId"] == f) {
                return params["url"].as_str().map(str::to_string);
            }
        }
        None
    };
    tokio::time::timeout(WITHIN_DOCUMENT_WAIT, moved)
        .await
        .ok()
        .flatten()
}

/// Moves `target` one entry `step` in its history and waits (at most
/// [`LOAD_TIMEOUT`]) for the entry's document to be committed, then for it
/// to finish loading ([`crate::util::js::LOADED`]): a page restored from
/// the back-forward cache fires no `load`, and an entry of the same
/// document (`pushState`, a fragment) only `navigatedWithinDocument`.
async fn traverse_and_wait(
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
    step: HistoryStep,
) -> Result<NavigationOutcome, CallToolError> {
    let failure = "CDP Error";
    let history = crate::handler::send_or(target, "Page.getNavigationHistory", json!({}), failure)
        .await?
        .result
        .unwrap_or_default();
    let index = history["currentIndex"].as_i64().unwrap_or_default() + step.delta();
    let entry = usize::try_from(index)
        .ok()
        .filter(|&index| !is_creation_page(&history, index))
        .and_then(|index| history["entries"].get(index))
        .ok_or_else(|| CallToolError::from_message(step.missing()))?;
    let url = entry["url"].as_str().unwrap_or_default().to_string();
    // Subscribed first: the commit can come before the reply is read.
    let mut page_events = target.on_domain("Page");
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    let move_to = json!({"entryId": entry["id"]});
    crate::handler::send_or(target, "Page.navigateToHistoryEntry", move_to, failure).await?;
    let loaded = load_before(&mut page_events, deadline, is_commit).await
        && document_loaded(target, deadline).await;
    let outcome = finish(target, &url, json!({}), loaded).await;
    Ok(with_status(outcome, target, network).await)
}

/// Whether entry `index` of `history` (`Page.getNavigationHistory`) is the
/// page the tab was created with: its first entry, on about:blank or, for
/// the tab Chrome starts with, its new-tab page (#119). It is not a page of
/// the tab's history (#116): Firefox replaces it on the first navigation,
/// and open_tab drops it when it can (#112), but not once the page pushed
/// entries of its own while loading.
pub(crate) fn is_creation_page(history: &Value, index: usize) -> bool {
    let url = history["entries"][0]["url"].as_str().unwrap_or_default();
    index == 0 && (url == "about:blank" || NEW_TAB_PAGES.iter().any(|p| url.starts_with(p)))
}

/// The new-tab pages Chrome opens a tab on (its first window's tab); none
/// is a URL `navigate` loads.
const NEW_TAB_PAGES: [&str; 4] = [
    "chrome://new-tab-page/",
    "chrome://newtab/",
    "chrome://new-tab-page-third-party/",
    "chrome-search://local-ntp/",
];

/// Whether `event` commits a new document in the top frame, or moves
/// within the current one.
fn is_commit(event: &cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>) -> bool {
    event.as_ref().is_ok_and(|e| match e.method.as_deref() {
        Some("Page.navigatedWithinDocument") => true,
        Some("Page.frameNavigated") => e
            .params
            .as_ref()
            .is_some_and(|p| p.pointer("/frame/parentId").is_none()),
        _ => false,
    })
}

/// Whether the tab's document finished loading before `deadline`.
async fn document_loaded(target: &CdpTarget, deadline: tokio::time::Instant) -> bool {
    let params =
        json!({"expression": crate::util::js::LOADED, "awaitPromise": true, "returnByValue": true});
    let wait = target.send_raw_command("Runtime.evaluate", params);
    matches!(tokio::time::timeout_at(deadline, wait).await, Ok(Ok(_)))
}

/// Fails on the `errorText` of a `Page.navigate` reply, saying so for a
/// download. An error status with an empty body
/// (`ERR_HTTP_RESPONSE_CODE_FAILURE`, Chrome then shows its own page) is
/// not a failure when the response is logged: the server answered, and the
/// reply carries its status, as on Firefox.
async fn check_error(
    browser: &Value,
    network: &Mutex<NetworkState>,
    url: &str,
) -> Result<(), CallToolError> {
    let text = |key: &str| browser.get(key).and_then(Value::as_str);
    let Some(reason) = text("errorText").filter(|e| !e.is_empty()) else {
        return Ok(());
    };
    if browser.get("isDownload").and_then(Value::as_bool) == Some(true) {
        return Err(navigation_failed(DOWNLOAD, url));
    }
    let answered = reason == "net::ERR_HTTP_RESPONSE_CODE_FAILURE"
        && document_response(network, text("loaderId"), url)
            .await
            .is_some();
    if answered {
        return Ok(());
    }
    if reason == "net::ERR_INVALID_AUTH_CREDENTIALS" {
        let document = document_response(network, text("loaderId"), url).await;
        let challenge = document
            .as_ref()
            .and_then(|d| d.response_header("www-authenticate"));
        return Err(navigation_failed(
            &crate::handler::navigation::auth_required(challenge),
            url,
        ));
    }
    Err(navigation_failed(reason, url))
}

fn navigation_failed(reason: &str, url: &str) -> CallToolError {
    BackendError::Navigation {
        reason: reason.to_string(),
        url: url.to_string(),
    }
    .into()
}

/// The outcome once the page settled: its committed URL, plus a note when
/// `load` did not fire in time.
pub(crate) async fn finish(
    target: &CdpTarget,
    requested: &str,
    browser: Value,
    loaded: bool,
) -> NavigationOutcome {
    let url = committed_url(target)
        .await
        .unwrap_or_else(|| requested.to_string());
    let notes = if loaded {
        Vec::new()
    } else {
        vec![NOT_LOADED_NOTE.to_string()]
    };
    NavigationOutcome {
        url,
        browser,
        notes,
        status: None,
    }
}

/// `outcome` with its document's HTTP status, read from the tab's request
/// log `network` (the document request id is its loader id).
pub(crate) async fn with_status(
    outcome: NavigationOutcome,
    target: &CdpTarget,
    network: &Mutex<NetworkState>,
) -> NavigationOutcome {
    let loader = match outcome.browser.get("loaderId").and_then(Value::as_str) {
        Some(id) => Some(id.to_string()),
        None => main_loader(target).await,
    };
    let document = document_response(network, loader.as_deref(), &outcome.url).await;
    outcome.with_status(document.and_then(|d| d.response_status))
}

/// The loader id of the tab's current document (its document request id),
/// for replies that carry none (`Page.reload`, a timed-out navigation).
async fn main_loader(target: &CdpTarget) -> Option<String> {
    let reply = target
        .send_raw_command("Page.getFrameTree", json!({}))
        .await
        .ok()?
        .result?;
    let loader = reply.pointer("/frameTree/frame/loaderId")?.as_str()?;
    Some(loader.to_string())
}

/// Waits for `Page.loadEventFired` on `events`; `false` after [`LOAD_TIMEOUT`].
pub(crate) async fn wait_for_load(events: &mut cdp_browser_lite::EventFilter) -> bool {
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    load_before(events, deadline, is_load).await
}

/// Waits (at most [`LOAD_TIMEOUT`]) for the `load` of the document the
/// navigation of `loader` commits: only after its `Page.frameNavigated`.
/// A `load` before that is the previous document's, still loading when the
/// navigation started; answering on it let the agent's next navigation
/// cancel the new page before it was in the history (ISSUE-94).
async fn wait_for_load_of(
    events: &mut (
             impl tokio_stream::Stream<Item = cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>>
             + Unpin
         ),
    loader: &str,
) -> bool {
    // Atomic, not `Cell`: the wait is held across awaits in `Send` futures.
    let committed = std::sync::atomic::AtomicBool::new(false);
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    let ours = |event: &cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>| {
        let Ok(event) = event else { return false };
        match event.method.as_deref() {
            Some("Page.frameNavigated") => {
                let params = event.params.as_ref();
                let id = params.and_then(|p| p.pointer("/frame/loaderId"));
                if id.and_then(Value::as_str) == Some(loader) {
                    committed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                false
            }
            Some("Page.loadEventFired") => committed.load(std::sync::atomic::Ordering::Relaxed),
            _ => false,
        }
    };
    load_before(events, deadline, ours).await
}

fn is_load(event: &cdp_browser_lite::CdpResult<cdp_browser_lite::WsResponse>) -> bool {
    event
        .as_ref()
        .is_ok_and(|e| e.method.as_deref() == Some("Page.loadEventFired"))
}

/// The URL of the current history entry (the committed document, after
/// redirects), read without running script in the page.
async fn committed_url(target: &CdpTarget) -> Option<String> {
    let reply = target
        .send_raw_command("Page.getNavigationHistory", json!({}))
        .await
        .ok()?
        .result?;
    let index = reply.get("currentIndex")?.as_u64()?;
    let entry = reply.get("entries")?.get(usize::try_from(index).ok()?)?;
    entry.get("url")?.as_str().map(str::to_string)
}

/// The message asking the agent to get the user's consent before a seeded
/// relaunch, listing the tabs that would be closed.
fn restart_warning(instance_id: &str, session: &BrowserSession) -> String {
    let registry = crate::util::sync::read(&session.tabs);
    let tab_list = registry
        .tabs
        .iter()
        .map(|(id, entry)| {
            let label = entry
                .label
                .as_ref()
                .map(|l| format!(" \"{l}\""))
                .unwrap_or_default();
            let active = if registry.active_tab_id.as_ref() == Some(id) {
                " (active)"
            } else {
                ""
            };
            format!("  - {id}{label}{active}: {}", entry.url)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tab_count = registry.tabs.len();
    let tab_info = if tab_count > 0 {
        format!(
            "its {tab_count} open tab(s), discarding their state (unsaved form input, breakpoints, and captured logs):\n{tab_list}"
        )
    } else {
        "any open tabs".to_string()
    };
    format!(
        "Cookie import requires relaunching instance '{instance_id}' with a seeded profile: a running browser cannot pick up cookies from disk.\n\nThis will CLOSE the instance and {tab_info}\n\nTell the user exactly which tabs will be closed and ask them to confirm. Only if they agree, call navigate again with the same arguments plus `confirm_restart: true`."
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
    use crate::handler::chrome_instance::MockChromeManager;
    use rust_mcp_sdk::schema::CallToolRequestParams;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_navigate_params_deserialization() {
        let params: Result<CallToolRequestParams, _> = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com"
            }
        }));
        assert!(params.is_ok());
    }

    #[tokio::test]
    async fn test_navigate_tool_deserialization() {
        let tool: Result<NavigateTool, _> = serde_json::from_value(json!({
            "url": "https://example.com"
        }));
        assert!(tool.is_ok());
        assert_eq!(tool.unwrap().url.as_deref(), Some("https://example.com"));
    }

    use crate::handler::cdp_domains::tests::{
        call, committed_url, fire_events_after, mock_handler, reply_text, spawn_recording_mock,
    };
    use crate::handler::navigation::DOWNLOAD;

    /// The JSON object after the first line of a reply.
    fn body(reply: &CallToolResult) -> Value {
        let text = reply_text(reply);
        serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap()
    }

    /// A tab two entries long whose current entry is `current` (0 or 1);
    /// after `Page.navigateToHistoryEntry` the entry it named is current.
    fn two_entry_history(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        current: u64,
    ) {
        history_of(
            mock,
            current,
            ["http://127.0.0.1:8/one", "http://127.0.0.1:8/two"],
        );
    }

    /// [`two_entry_history`] with the entries' `urls`.
    fn history_of(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        current: u64,
        urls: [&'static str; 2],
    ) {
        let index = Arc::new(std::sync::atomic::AtomicU64::new(current));
        let read = index.clone();
        mock.respond_with("Page.getNavigationHistory", move |_| {
            let current = read.load(std::sync::atomic::Ordering::SeqCst);
            let entries =
                [(7, urls[0]), (8, urls[1])].map(|(id, url)| json!({"id": id, "url": url}));
            json!({"currentIndex": current, "entries": entries})
        });
        mock.respond_with("Page.navigateToHistoryEntry", move |params| {
            let id = params["entryId"].as_u64().unwrap_or_default();
            index.store(id - 7, std::sync::atomic::Ordering::SeqCst);
            json!({})
        });
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "boolean", "value": true}}}),
        );
        let committed = json!({"frame": {"id": "F1", "loaderId": "L7", "url": "", "securityOrigin": "", "mimeType": "text/html"}, "type": "Navigation"});
        fire_events_after(
            mock,
            "Page.navigateToHistoryEntry",
            vec![("Page.frameNavigated", committed)],
        );
    }

    /// ISSUE-31: history 'back' moves to the previous entry, waits for the
    /// new document and answers like a URL navigation.
    #[tokio::test]
    async fn history_back_goes_to_the_previous_entry() {
        // Given a tab on its second entry
        let mock = spawn_recording_mock().await;
        two_entry_history(mock, 1);
        let handler = mock_handler(mock.http_port);
        // When it goes back
        let args = json!({"history": "back"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the first entry is loaded and reported
        let sent = &mock.commands_named("Page.navigateToHistoryEntry")[0].params;
        assert_eq!(sent["entryId"], 7);
        assert!(
            reply_text(&reply).starts_with("Went back to http://127.0.0.1:8/one."),
            "{}",
            reply_text(&reply)
        );
        assert!(body(&reply).get("notes").is_none(), "{}", body(&reply));
    }

    /// NAV-09 (#116): on Chrome a blank first entry is the page the tab was
    /// created with, not a page of its history (Firefox replaces it on the
    /// first navigation): 'back' to it fails without moving, as on Firefox.
    #[tokio::test]
    async fn history_back_never_reaches_the_blank_first_entry() {
        // Given a tab on the page it loaded after its blank creation page
        let mock = spawn_recording_mock().await;
        history_of(mock, 1, ["about:blank", "http://127.0.0.1:8/one"]);
        let handler = mock_handler(mock.http_port);
        // When it goes back
        let back = json!({"history": "back"});
        let error = NavigateTool::handle(call("navigate", back), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails without moving
        assert!(error.contains("no previous page"), "{error}");
        assert!(
            mock.commands_named("Page.navigateToHistoryEntry")
                .is_empty()
        );
    }

    /// NAV-10 (#119): the tab Chrome starts with begins on its new-tab page
    /// (chrome://new-tab-page/), not about:blank: 'back' to it fails too.
    #[tokio::test]
    async fn history_back_never_reaches_the_new_tab_page() {
        // Given the initial tab on the page it loaded after its new-tab page
        let mock = spawn_recording_mock().await;
        history_of(
            mock,
            1,
            ["chrome://new-tab-page/", "http://127.0.0.1:8/one"],
        );
        let handler = mock_handler(mock.http_port);
        // When it goes back
        let back = json!({"history": "back"});
        let error = NavigateTool::handle(call("navigate", back), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails without moving
        assert!(error.contains("no previous page"), "{error}");
        assert!(
            mock.commands_named("Page.navigateToHistoryEntry")
                .is_empty()
        );
    }

    /// NAV-10 (#119): only a first entry on a blank or new-tab page is the
    /// tab's creation page; a page the agent loaded is not.
    #[test]
    fn creation_pages_are_blank_or_new_tab_first_entries() {
        let history = |url: &str| json!({"entries": [{"url": url}, {"url": url}]});
        for url in [
            "about:blank",
            "chrome://new-tab-page/",
            "chrome://newtab/",
            "chrome://new-tab-page-third-party/",
            "chrome-search://local-ntp/local-ntp.html",
        ] {
            assert!(is_creation_page(&history(url), 0), "{url}");
            assert!(!is_creation_page(&history(url), 1), "{url}");
        }
        for url in ["http://127.0.0.1:8/", "chrome://settings/", "about:blank#x"] {
            assert!(!is_creation_page(&history(url), 0), "{url}");
        }
    }

    /// ISSUE-31: at the last entry 'forward' fails without moving; a call
    /// with both or neither of url and history fails too.
    #[tokio::test]
    async fn history_at_its_end_does_not_move() {
        let mock = spawn_recording_mock().await;
        two_entry_history(mock, 1);
        let handler = mock_handler(mock.http_port);
        let forward = json!({"history": "forward"});
        let error = NavigateTool::handle(call("navigate", forward), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no next page"), "{error}");
        for args in [
            json!({}),
            json!({"url": "http://127.0.0.1:8/x", "history": "back"}),
        ] {
            let error = NavigateTool::handle(call("navigate", args), &handler)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("either 'url' or 'history'"), "{error}");
        }
        assert!(
            mock.commands_named("Page.navigateToHistoryEntry")
                .is_empty()
        );
        assert!(mock.commands_named("Page.navigate").is_empty());
    }

    /// NAV-01 / NAV-02: navigate waits for `load` and answers the final URL
    /// (after redirects) in the shared shape.
    #[tokio::test]
    async fn navigate_reports_final_url_after_load() {
        // Given a Chrome whose navigation to /start ends on /final
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        committed_url(mock, "http://127.0.0.1:8/final");
        fire_events_after(mock, "Page.navigate", vec![committed("L1"), load_event()]);
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/start"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the reply has the final URL, the tab and the CDP ids
        assert!(reply_text(&reply).starts_with("Navigated to http://127.0.0.1:8/final."));
        let body = body(&reply);
        assert_eq!(body["url"], "http://127.0.0.1:8/final");
        assert_eq!(body["browser"]["loaderId"], "L1");
        assert!(body.get("tab_id").is_some());
        assert!(body.get("notes").is_none(), "load fired: {body}");
    }

    /// #133: the tab's Network domain is enabled on the navigation's own
    /// session right before it: a fresh tab is registered (and navigated)
    /// while its domains are still being enabled, and the first page's
    /// requests went unrecorded.
    #[tokio::test]
    async fn network_is_enabled_right_before_the_navigation() {
        // Given a Chrome that loads the page
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        committed_url(mock, "http://127.0.0.1:8/");
        fire_events_after(mock, "Page.navigate", vec![committed("L1"), load_event()]);
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/"});
        NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the navigation's session had Network enabled just before it
        let commands = mock.commands();
        let navigation = commands.iter().position(|c| c.method == "Page.navigate");
        let navigation = navigation.unwrap();
        let session = &commands[navigation].session_id;
        let before = commands[..navigation]
            .iter()
            .rev()
            .find(|c| &c.session_id == session);
        assert_eq!(before.map(|c| c.method.as_str()), Some("Network.enable"));
    }

    /// NAV-05 (#105): with --local, a navigation that a redirect takes to an
    /// address that is not local fails naming --local and that URL, and the
    /// tab is sent back to about:blank (Chrome followed the redirect before
    /// the server could see it).
    #[tokio::test]
    async fn local_mode_refuses_a_redirect_to_a_remote_address() {
        // Given a --local Chrome whose navigation to a local URL ends remote
        // (about:blank answered without a loader: the mock fires the events
        // of the first navigation only)
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |params| match params["url"].as_str() {
            Some("about:blank") => json!({"frameId": "F1"}),
            _ => json!({"frameId": "F1", "loaderId": "L1"}),
        });
        committed_url(mock, "http://127.0.0.2:8/m.html");
        fire_events_after(mock, "Page.navigate", vec![committed("L1"), load_event()]);
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/redirect"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming --local and the remote origin (not its path)
        assert!(
            error.contains("--local") && error.ends_with("(http://127.0.0.2:8)"),
            "{error}"
        );
        // And the tab was sent back to about:blank
        let sent = mock.commands_named("Page.navigate");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].params["url"], "about:blank");
    }

    /// NAV-05 (#105): the tool answers only once about:blank has loaded (by
    /// its loader): a tool called right after met "Not attached to an
    /// active page" while Chrome was still committing it.
    #[tokio::test]
    async fn local_mode_answers_once_about_blank_has_loaded() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Given a --local Chrome whose navigation ends remote, and whose
        // about:blank loads 200 ms after Chrome answers its Page.navigate
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |params| match params["url"].as_str() {
            Some("about:blank") => json!({"frameId": "F1", "loaderId": "L2"}),
            _ => json!({"frameId": "F1", "loaderId": "L1"}),
        });
        committed_url(mock, "http://127.0.0.2:8/m.html");
        fire_events_after(mock, "Page.navigate", vec![committed("L1"), load_event()]);
        let blank_loaded = Arc::new(AtomicBool::new(false));
        let loaded = blank_loaded.clone();
        tokio::spawn(async move {
            while mock.commands_named("Page.navigate").len() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let session = mock.commands_named("Page.navigate")[1].session_id.clone();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            loaded.store(true, Ordering::SeqCst);
            for (event, params) in [committed("L2"), load_event()] {
                mock.emit(event, params, session.as_deref());
            }
        });
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        // When navigate runs
        let started = std::time::Instant::now();
        let args = json!({"url": "http://127.0.0.1:8/redirect"});
        let error = NavigateTool::handle(call("navigate", args), &handler).await;
        // Then it failed after about:blank loaded, not at the wait's bound
        assert!(error.is_err());
        assert!(
            blank_loaded.load(Ordering::SeqCst),
            "answered before about:blank loaded"
        );
        assert!(started.elapsed() < BLANK_WAIT, "{:?}", started.elapsed());
    }

    /// NAV-06 (#105): with --local, a history move to an entry that is not
    /// local (left there by a redirect before the tab was sent back to
    /// about:blank) fails the same way.
    #[tokio::test]
    async fn local_mode_refuses_a_history_entry_on_a_remote_address() {
        // Given a --local tab whose previous entry is remote
        let mock = spawn_recording_mock().await;
        history_of(mock, 1, ["http://127.0.0.2:8/m.html", "about:blank"]);
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        // When it goes back
        let args = json!({"history": "back"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming --local, and the tab is sent to about:blank
        assert!(
            error.contains("--local") && error.ends_with("(http://127.0.0.2:8)"),
            "{error}"
        );
        let sent = mock.commands_named("Page.navigate");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].params["url"], "about:blank");
    }

    /// A --local Chrome whose navigation commits `landed`, where the page
    /// opens an alert ("PageText") while loading; about:blank answers
    /// without a loader.
    fn alert_while_loading(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        landed: &'static str,
    ) -> McpHandler {
        mock.respond_with("Page.navigate", |params| match params["url"].as_str() {
            Some("about:blank") => json!({"frameId": "F1"}),
            _ => json!({"frameId": "F1", "loaderId": "L1"}),
        });
        mock.respond_with("Page.handleJavaScriptDialog", |_| json!({}));
        committed_url(mock, landed);
        let alert = json!({"url": landed, "message": "PageText", "type": "alert", "hasBrowserHandler": false, "defaultPrompt": ""});
        let opening = "Page.javascriptDialogOpening";
        crate::handler::cdp_domains::tests::fire_after(mock, "Page.navigate", opening, alert);
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        handler
    }

    /// NAV-07 (#117): with --local, a redirect to a page that is not local
    /// and opens a dialog while loading fails as --local, not quoting the
    /// dialog (the remote page's text): the dialog is dismissed and the tab
    /// sent back to about:blank, so no later tool reads that page.
    #[tokio::test]
    async fn local_mode_refuses_a_remote_page_that_opens_a_dialog() {
        // Given a --local Chrome redirected to a remote page that alerts
        let mock = spawn_recording_mock().await;
        let handler = alert_while_loading(mock, "http://127.0.0.2:8/alert.html");
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/redirect"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming --local and the remote origin, not the dialog
        assert!(
            error.contains("--local") && error.ends_with("(http://127.0.0.2:8)"),
            "{error}"
        );
        assert!(!error.contains("PageText"), "{error}");
        // And the dialog was dismissed, then the tab sent to about:blank
        let dismissed = mock.commands_named("Page.handleJavaScriptDialog");
        assert_eq!(dismissed.len(), 1);
        assert_eq!(dismissed[0].params["accept"], false);
        let sent = mock.commands_named("Page.navigate");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].params["url"], "about:blank");
    }

    /// NAV-07 (#117): a local page's dialog still fails navigate quoting
    /// it, and stays open for handle_dialog.
    #[tokio::test]
    async fn local_mode_keeps_a_local_pages_dialog() {
        // Given a --local Chrome on a local page that alerts while loading
        let mock = spawn_recording_mock().await;
        let handler = alert_while_loading(mock, "http://127.0.0.1:8/alert.html");
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/alert.html"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails quoting the dialog, which nothing closed
        assert!(error.contains("PageText"), "{error}");
        assert!(
            mock.commands_named("Page.handleJavaScriptDialog")
                .is_empty()
        );
        assert_eq!(mock.commands_named("Page.navigate").len(), 1);
    }

    /// A --local Chrome whose navigation commits `landed`, where the page
    /// pauses in the debugger while loading (a pause it keeps); about:blank
    /// answers without a loader.
    fn pause_while_loading(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        landed: &'static str,
    ) -> McpHandler {
        mock.respond_with("Page.navigate", |params| match params["url"].as_str() {
            Some("about:blank") => json!({"frameId": "F1"}),
            _ => json!({"frameId": "F1", "loaderId": "L1"}),
        });
        committed_url(mock, landed);
        pause_after(mock, "Page.navigate");
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        handler
    }

    /// The `Debugger.resume` commands that end the paused script.
    fn terminating_resumes(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
    ) -> usize {
        let resumes = mock.commands_named("Debugger.resume");
        let ending = resumes
            .iter()
            .filter(|c| c.params["terminateOnResume"] == true);
        ending.count()
    }

    /// NAV-07 (#124): with --local, a redirect to a page that is not local
    /// and pauses in the debugger while loading fails as --local, not
    /// naming the pause: the pause is resumed, ending its script, and the
    /// tab sent back to about:blank, so `resume` does not let it load.
    #[tokio::test]
    async fn local_mode_refuses_a_remote_page_that_pauses() {
        // Given a --local Chrome redirected to a remote page that pauses
        let mock = spawn_recording_mock().await;
        let handler = pause_while_loading(mock, "http://127.0.0.2:8/pause.html");
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/redirect"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming --local and the remote origin, not the pause
        assert!(
            error.contains("--local") && error.ends_with("(http://127.0.0.2:8)"),
            "{error}"
        );
        assert!(!error.contains("paused in the debugger"), "{error}");
        assert!(error.contains("sent back to about:blank"), "{error}");
        // And the pause was ended before the tab was sent to about:blank
        assert_eq!(terminating_resumes(mock), 1);
        let sent = mock.commands_named("Page.navigate");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].params["url"], "about:blank");
        let all = mock.commands();
        let ended = all
            .iter()
            .position(|c| c.method == "Debugger.resume" && c.params["terminateOnResume"] == true);
        let blanked = all
            .iter()
            .position(|c| c.method == "Page.navigate" && c.params["url"] == "about:blank");
        assert!(ended.unwrap() < blanked.unwrap());
    }

    /// NAV-07 (#124): a local page's pause still fails navigate naming it,
    /// and stays for the agent to inspect.
    #[tokio::test]
    async fn local_mode_keeps_a_local_pages_pause() {
        // Given a --local Chrome on a local page that pauses while loading
        let mock = spawn_recording_mock().await;
        let handler = pause_while_loading(mock, "http://127.0.0.1:8/pause.html");
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/pause.html"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails naming the pause, which nothing ended
        assert!(error.contains("paused in the debugger"), "{error}");
        assert_eq!(terminating_resumes(mock), 0);
        assert_eq!(mock.commands_named("Page.navigate").len(), 1);
    }

    /// NAV-07 (#123): with --local, a remote page that opens another dialog
    /// while the tab is sent back to about:blank (a burst of alerts) does
    /// not keep it there: the dialog is dismissed and the blanking tried
    /// again (one try said it failed, and the page stayed readable).
    #[tokio::test]
    async fn local_mode_blanks_again_after_a_dialog_interrupts_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Given a --local Chrome redirected to a remote page, which alerts
        // while about:blank loads the first time
        let mock = spawn_recording_mock().await;
        let blanks = AtomicUsize::new(0);
        mock.respond_with("Page.navigate", move |params| {
            if params["url"] != "about:blank" {
                return json!({"frameId": "F1", "loaderId": "L1"});
            }
            if blanks.fetch_add(1, Ordering::SeqCst) > 0 {
                return json!({"frameId": "F1"});
            }
            let sent = mock.commands_named("Page.navigate");
            let session = sent.last().and_then(|c| c.session_id.clone());
            let alert = json!({"type": "alert", "message": "PageText", "url": "http://127.0.0.2:8/burst.html"});
            mock.emit("Page.javascriptDialogOpening", alert, session.as_deref());
            json!({"frameId": "F1", "loaderId": "L-blank"})
        });
        mock.respond_with("Page.handleJavaScriptDialog", |_| json!({}));
        committed_url(mock, "http://127.0.0.2:8/burst.html");
        fire_events_after(mock, "Page.navigate", vec![committed("L1"), load_event()]);
        let mut handler = mock_handler(mock.http_port);
        handler.flags.local = true;
        // When navigate runs
        let started = std::time::Instant::now();
        let args = json!({"url": "http://127.0.0.1:8/redirect"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails as --local, the tab sent back to about:blank
        assert!(
            error.contains("--local") && error.contains("sent back to about:blank"),
            "{error}"
        );
        assert!(!error.contains("PageText"), "{error}");
        // On the second try, after the new dialog was dismissed, at once
        let sent = mock.commands_named("Page.navigate");
        let blank: Vec<_> = sent
            .iter()
            .filter(|c| c.params["url"] == "about:blank")
            .collect();
        assert_eq!(blank.len(), 2);
        assert_eq!(mock.commands_named("Page.handleJavaScriptDialog").len(), 2);
        assert!(started.elapsed() < BLANK_WAIT, "{:?}", started.elapsed());
    }

    /// ISSUE-94: only the `load` of the document the navigation commits
    /// (its `frameNavigated` carries the reply's loader) ends the wait; a
    /// `load` before it is the previous document's, still loading when the
    /// navigation started: answering then let the next navigation cancel
    /// the page before it was in the history.
    #[tokio::test]
    async fn only_the_new_documents_load_ends_the_wait() {
        use crate::handler::cdp_domains::tests::make_event;
        let load = || Ok(make_event("Page.loadEventFired", json!({"timestamp": 1.0})));
        let navigated = |loader: &str| {
            let frame = json!({"frame": {"id": "F1", "loaderId": loader, "url": "http://a/"}});
            Ok(make_event("Page.frameNavigated", frame))
        };
        // Given the previous document's load, then an iframe's commit
        let stale = vec![load(), navigated("L-iframe"), load()];
        // When the wait for loader L2 reads them, they do not end it
        let mut events = tokio_stream::iter(stale);
        assert!(!wait_for_load_of(&mut events, "L2").await);
        // But L2's commit followed by a load does
        let fresh = vec![load(), navigated("L2"), load()];
        let mut events = tokio_stream::iter(fresh);
        assert!(wait_for_load_of(&mut events, "L2").await);
    }

    /// ISSUE-70: a navigation to a fragment of the current page answers
    /// the URL Chrome's `navigatedWithinDocument` gives for the page's own
    /// frame, not the history entry, which may still hold the previous URL.
    #[tokio::test]
    async fn fragment_navigation_reports_the_new_url() {
        // Given a same-document navigation whose history entry is stale, and
        // an iframe that also moves within its document
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| json!({"frameId": "F1"}));
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_events_after(
            mock,
            "Page.navigate",
            vec![
                (
                    "Page.navigatedWithinDocument",
                    json!({"frameId": "F2", "url": "http://127.0.0.1:8/frame#x"}),
                ),
                (
                    "Page.navigatedWithinDocument",
                    json!({"frameId": "F1", "url": "http://127.0.0.1:8/page#/contact"}),
                ),
            ],
        );
        let handler = mock_handler(mock.http_port);
        // When navigate goes to the fragment
        let args = json!({"url": "http://127.0.0.1:8/page#/contact"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the reply has the page's new URL
        assert_eq!(body(&reply)["url"], "http://127.0.0.1:8/page#/contact");
        assert!(reply_text(&reply).starts_with("Navigated to http://127.0.0.1:8/page#/contact."));
    }

    /// The `Debugger.paused` of a page stopped at line 2 of app.js while it
    /// loads, emitted after the next `command`.
    pub(crate) fn pause_after(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        command: &'static str,
    ) {
        let location = json!({"scriptId": "7", "lineNumber": 2, "columnNumber": 4});
        let frame =
            json!({"callFrameId": "c1", "url": "http://127.0.0.1:8/app.js", "location": location});
        let paused = json!({"reason": "other", "callFrames": [frame]});
        crate::handler::cdp_domains::tests::fire_after(mock, command, "Debugger.paused", paused);
    }

    /// ISSUE-45: a page that pauses in the debugger while it loads makes
    /// navigate fail at once naming where (it waited 30 s for `load`).
    #[tokio::test]
    async fn a_pause_while_loading_ends_the_wait() {
        // Given a navigation that pauses at app.js before `load`
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        pause_after(mock, "Page.navigate");
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let started = std::time::Instant::now();
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails at once, naming the pause and the call
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            error.contains("paused in the debugger at http://127.0.0.1:8/app.js line 2"),
            "{error}"
        );
        assert!(error.contains("while running navigate"), "{error}");
    }

    /// The CDP events of a document response with `status` on loader `L1`.
    fn document_events(status: i64) -> Vec<(&'static str, Value)> {
        let url = "http://127.0.0.1:8/page";
        vec![
            (
                "Network.requestWillBeSent",
                json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "request": {"url": url, "method": "GET", "headers": {}}}),
            ),
            (
                "Network.responseReceived",
                json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "response": {"url": url, "status": status, "statusText": "", "headers": {}}}),
            ),
            committed("L1"),
            load_event(),
        ]
    }

    /// Chrome's `Page.frameNavigated` for the top document of `loader`.
    fn committed(loader: &str) -> (&'static str, Value) {
        let frame = json!({"id": "F1", "loaderId": loader, "url": "http://127.0.0.1:8/page"});
        (
            "Page.frameNavigated",
            json!({"frame": frame, "type": "Navigation"}),
        )
    }

    /// Chrome's `Page.loadEventFired`.
    fn load_event() -> (&'static str, Value) {
        ("Page.loadEventFired", json!({"timestamp": 1.0}))
    }

    /// AUD-EV-11: the document's HTTP status is in the reply; an error
    /// status adds a note.
    #[tokio::test]
    async fn navigate_reports_the_http_status() {
        // Given a server answering 404
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.navigate",
            |_| json!({"frameId": "F1", "loaderId": "L1"}),
        );
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_events_after(mock, "Page.navigate", document_events(404));
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then the reply has the status and says it is an error
        let body = body(&reply);
        assert_eq!(body["status"], 404, "{body}");
        assert_eq!(body["notes"][0], "the server answered HTTP 404");
    }

    /// AUD-EV-11: an error status with an empty body is a navigation that
    /// reached the server (Chrome reports ERR_HTTP_RESPONSE_CODE_FAILURE and
    /// shows its own page; Firefox shows the empty page).
    #[tokio::test]
    async fn empty_error_page_is_reported_with_its_status() {
        // Given a server answering 500 with no body
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_HTTP_RESPONSE_CODE_FAILURE"})
        });
        committed_url(mock, "http://127.0.0.1:8/page");
        fire_events_after(mock, "Page.navigate", document_events(500));
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let reply = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap();
        // Then it succeeds with the status
        let body = body(&reply);
        assert_eq!(body["status"], 500, "{body}");
        assert_eq!(body["notes"][0], "the server answered HTTP 500");
    }

    /// AUD-EV-21: a URL the browser downloads fails, saying so (same on Firefox).
    #[tokio::test]
    async fn download_fails_saying_so() {
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_ABORTED", "isDownload": true})
        });
        let handler = mock_handler(mock.http_port);
        let args = json!({"url": "http://127.0.0.1:8/a.bin"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Navigation failed: {DOWNLOAD} (http://127.0.0.1:8/a.bin)")
        );
    }

    /// ISSUE-50: a page behind HTTP authentication fails saying so, with
    /// its challenge from the logged response (Chrome only said
    /// net::ERR_INVALID_AUTH_CREDENTIALS).
    #[tokio::test]
    async fn authentication_fails_saying_so() {
        // Given a page answering 401 with a Basic challenge
        let mock = spawn_recording_mock().await;
        let url = "http://127.0.0.1:8/auth";
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_INVALID_AUTH_CREDENTIALS"})
        });
        let headers = json!({"WWW-Authenticate": "Basic realm=\"bench\""});
        fire_events_after(
            mock,
            "Page.navigate",
            vec![
                (
                    "Network.requestWillBeSent",
                    json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "request": {"url": url, "method": "GET", "headers": {}}}),
                ),
                (
                    "Network.responseReceived",
                    json!({"requestId": "L1", "loaderId": "L1", "type": "Document", "response": {"url": url, "status": 401, "statusText": "Unauthorized", "headers": headers}}),
                ),
            ],
        );
        let handler = mock_handler(mock.http_port);
        // When navigate runs, Then it names the authentication
        let error = NavigateTool::handle(call("navigate", json!({"url": url})), &handler)
            .await
            .unwrap_err()
            .to_string();
        let expected = crate::handler::navigation::auth_required(Some("Basic realm=\"bench\""));
        assert_eq!(error, format!("Navigation failed: {expected} ({url})"));
    }

    /// NAV-03: a network error fails the call (Chrome reports it inside a
    /// successful Page.navigate reply).
    #[tokio::test]
    async fn unknown_host_fails_navigation() {
        // Given a host Chrome cannot resolve
        let mock = spawn_recording_mock().await;
        mock.respond_with("Page.navigate", |_| {
            json!({"frameId": "F1", "loaderId": "L1", "errorText": "net::ERR_NAME_NOT_RESOLVED"})
        });
        let handler = mock_handler(mock.http_port);
        // When navigate runs
        let args = json!({"url": "http://nonexistent-host.invalid/"});
        let error = NavigateTool::handle(call("navigate", args), &handler)
            .await
            .unwrap_err();
        // Then it fails with the network error and the URL
        assert_eq!(
            error.to_string(),
            "Navigation failed: net::ERR_NAME_NOT_RESOLVED (http://nonexistent-host.invalid/)"
        );
    }

    #[tokio::test]
    async fn test_navigate_handle() {
        let port = spawn_mock_chrome_server().await;

        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com"
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_ok(), "Handle should succeed: {:?}", result.err());

        let call_result = result.unwrap();
        assert!(!call_result.content.is_empty());
        let content_str = format!("{:?}", call_result.content);
        assert!(
            content_str.contains("Navigated to https://example.com"),
            "Content didn't match: {}",
            content_str
        );
    }

    #[tokio::test]
    async fn test_navigate_local_only_restriction() {
        let mut handler = McpHandler::new_test();
        handler.flags.local = true;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://google.com"
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Navigation to 'https://google.com' is blocked")
        );

        // Test with local address
        let params_local: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "http://localhost:3000"
            }
        }))
        .unwrap();

        let port = spawn_mock_chrome_server().await;
        let mut handler_local = McpHandler::new_test_with_port(port);
        handler_local.flags.local = true;

        let result_local = NavigateTool::handle(params_local, &handler_local).await;
        assert!(
            result_local.is_ok(),
            "Local navigation should succeed: {:?}",
            result_local.err()
        );
    }

    #[tokio::test]
    async fn test_navigate_local_only_addresses() {
        use crate::handler::cdp_domains::tests::{fire_events_after, spawn_recording_mock};
        let mut handler = McpHandler::new_test();
        handler.flags.local = true;
        let mock = spawn_recording_mock().await;
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(mock.http_port)));

        let local_urls = vec![
            "http://127.0.0.1:8080",
            "http://localhost:5173",
            "http://192.168.1.50/index.html",
            "http://myapp.local/",
        ];

        for url in local_urls {
            // The mock's navigations have no loader: same-document ones.
            let moved = json!({"url": url});
            fire_events_after(
                mock,
                "Page.navigate",
                vec![("Page.navigatedWithinDocument", moved)],
            );
            let params: CallToolRequestParams = serde_json::from_value(json!({
                "name": "navigate",
                "arguments": {
                    "url": url
                }
            }))
            .unwrap();
            let result = NavigateTool::handle(params, &handler).await;
            assert!(result.is_ok(), "URL {} should be allowed", url);
        }

        let blocked_urls = vec![
            "https://github.com",
            "http://10.0.0.1", // Currently not in my list, following prompt's specific list
            "http://1.1.1.1",
        ];

        for url in blocked_urls {
            let params: CallToolRequestParams = serde_json::from_value(json!({
                "name": "navigate",
                "arguments": {
                    "url": url
                }
            }))
            .unwrap();
            let result = NavigateTool::handle(params, &handler).await;
            assert!(result.is_err(), "URL {} should be blocked", url);
        }
    }

    #[tokio::test]
    async fn test_navigate_cookie_import_disabled_by_default() {
        let handler = McpHandler::new_test();

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com",
                "copy_cookies": true
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Cookie import is disabled"));
    }

    #[tokio::test]
    async fn test_navigate_cookie_import_user_profile_error() {
        let mut handler = McpHandler::new_test();
        handler.flags.allow_cookie_import = true;
        handler.base_params.browser.user_profile = true;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "navigate",
            "arguments": {
                "url": "https://example.com",
                "copy_cookies": true
            }
        }))
        .unwrap();

        let result = NavigateTool::handle(params, &handler).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("redundant when running in --user-profile mode")
        );
    }

    /// TOL-08: Protocol replies are returned as JSON, not Rust Debug.
    #[tokio::test]
    async fn protocol_replies_are_json() {
        // Given a browser (mock) answering Page.navigate
        let port = spawn_mock_chrome_server().await;
        let mut handler = McpHandler::new_test();
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(Mutex::new(MockChromeManager::new(port)));
        // When navigate is called
        let params: CallToolRequestParams = serde_json::from_value(
            json!({"name": "navigate", "arguments": {"url": "https://example.com"}}),
        )
        .unwrap();
        let result = NavigateTool::handle(params, &handler).await.unwrap();
        // Then the text is the prefix followed by a JSON object
        let text = result.content[0].as_text_content().unwrap().text.clone();
        let (prefix, body) = text.split_once('\n').unwrap();
        assert_eq!(prefix, "Navigated to https://example.com.");
        assert!(
            serde_json::from_str::<serde_json::Value>(body)
                .unwrap()
                .is_object()
        );
        assert!(!text.contains("WsResponse"));
    }
}
