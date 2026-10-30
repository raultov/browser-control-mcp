//! The Firefox implementation of the tools. Each function parses the same
//! arguments as the Chrome tool (one schema per tool) and answers in the same
//! shape where the protocols allow it.

use super::debugger::{race_pause, unless_paused};
use super::page;
use super::session::FirefoxSession;
use crate::backend::BackendError;
use crate::backend::BrowserKind;
use crate::handler::cdp_domains::custom::get_custom_events::GetCustomEventsTool;
use crate::handler::cdp_domains::input::click_element::{
    ClickElementTool, click_point, clicked_reply,
};
use crate::handler::cdp_domains::input::fill_input::{
    FillInputTool, FillStart, fill_reply, fill_start, selected_reply,
};
use crate::handler::cdp_domains::log::get_console_logs::GetConsoleLogsTool;
use crate::handler::cdp_domains::network::get_network_logs::{self, GetNetworkLogsTool};
use crate::handler::cdp_domains::page::capture_screenshot::{self, CaptureScreenshotTool};
use crate::handler::cdp_domains::page::navigate::NavigateTool;
use crate::handler::cdp_domains::page::scroll::ScrollTool;
use crate::handler::cdp_domains::runtime::evaluate_js::EvaluateJsTool;
use crate::handler::cdp_domains::runtime::inspect_dom::InspectDomTool;
use crate::handler::chrome_instance::registry::InstanceDescriptor;
use crate::handler::chrome_instance::{close_instance, open_instance, open_tab, switch_tab};
use crate::handler::navigation::NavigationOutcome;
use crate::handler::{McpHandler, NetworkRequest, TargetArgs, parse_args};
use crate::util::sync::{read, write};
use bidi_browser_lite::Tab;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

type ToolResult = Result<CallToolResult, CallToolError>;

fn text(value: impl Into<String>) -> CallToolResult {
    CallToolResult::text_content(vec![value.into().into()])
}

/// The session and tab a call addresses, refused while a JavaScript dialog
/// is open on the tab (as on Chrome).
async fn target(
    handler: &McpHandler,
    ids: &TargetArgs,
) -> Result<(Arc<FirefoxSession>, String, super::session::FirefoxTab), CallToolError> {
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    let (id, tab) = session.tab(ids.tab_id.as_deref()).await?;
    let open = tab.dialog.lock().await.open.clone();
    if let Some(dialog) = open {
        return Err(CallToolError::from_message(dialog.blocking()));
    }
    Ok((session, id, tab))
}

/// `handle_dialog`: `browsingContext.handleUserPrompt` on the tab.
pub(crate) async fn handle_dialog(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    use crate::handler::cdp_domains::page::handle_dialog::HandleDialogTool;
    let args: HandleDialogTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (_, tab) = session.tab(args.tab_id.as_deref()).await?;
    let mut prompt = json!({"context": tab.tab.context_id(), "accept": args.accept});
    if let Some(text) = &args.prompt_text {
        prompt["userText"] = json!(text);
    }
    // Read first: closing it makes Firefox announce it closed.
    let dialog = tab.dialog.lock().await.open.clone();
    let reply = page::command(&tab.tab, "browsingContext.handleUserPrompt", prompt).await;
    tab.dialog.lock().await.open = None;
    match reply {
        Ok(_) => Ok(crate::handler::dialogs::handled(
            args.accept,
            dialog.as_ref(),
        )),
        Err(e) if e.to_string().contains("no such alert") => Err(CallToolError::from_message(
            crate::handler::dialogs::NO_DIALOG,
        )),
        Err(e) => Err(e.into()),
    }
}

fn ids(instance_id: &Option<String>, tab_id: &Option<String>) -> TargetArgs {
    TargetArgs {
        instance_id: instance_id.clone(),
        tab_id: tab_id.clone(),
    }
}

fn check_local(handler: &McpHandler, url: &str) -> Result<(), CallToolError> {
    if handler.flags.local && !crate::handler::is_local_address(url) {
        return Err(CallToolError::from_message(format!(
            "Navigation to '{url}' is blocked. This MCP server is running with --local, which restricts navigation to local addresses only (localhost, 127.0.0.1, 192.168.x.x, or *.local)."
        )));
    }
    Ok(())
}

// ---- page, runtime, input ----

/// `navigate`.
pub(crate) async fn navigate(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: NavigateTool = parse_args(params)?;
    check_local(handler, &args.url)?;
    let (session, id, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let navigation = page::navigate(&tab.tab, &args.url);
    let outcome = unless_paused(&session, &id, "navigate", navigation).await?;
    let document = navigation_document(&tab, &outcome, &args.url).await;
    if document.as_ref().is_some_and(NetworkRequest::is_attachment) {
        return Err(BackendError::Navigation {
            reason: crate::handler::navigation::DOWNLOAD.to_string(),
            url: args.url,
        }
        .into());
    }
    let outcome = outcome.with_status(document.and_then(|d| d.response_status));
    Ok(outcome.reply(&format!("Navigated to {}.", outcome.url), Some(&id)))
}

/// The logged document request of the navigation `outcome` reports (by
/// its BiDi `navigation` id), when `url` is http(s).
async fn navigation_document(
    tab: &super::session::FirefoxTab,
    outcome: &NavigationOutcome,
    url: &str,
) -> Option<NetworkRequest> {
    let navigation = outcome.browser.get("navigation").and_then(Value::as_str);
    crate::handler::navigation::document_response(&tab.network, navigation, url).await
}

/// `reload`.
pub(crate) async fn reload(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let (session, id, tab) = target(handler, &ids).await?;
    let outcome = unless_paused(&session, &id, "reload", page::reload(&tab.tab)).await?;
    let document = navigation_document(&tab, &outcome, &outcome.url).await;
    let outcome = outcome.with_status(document.and_then(|d| d.response_status));
    Ok(outcome.reply("Page reloaded.", Some(&id)))
}

/// Runs tool `name`, which runs page code on the tab `ids` address, failing
/// at once if the page pauses in the debugger meanwhile (`unless_paused`).
async fn on_page(
    handler: &McpHandler,
    ids: &TargetArgs,
    name: &str,
    call: impl AsyncFnOnce(&Tab) -> ToolResult,
) -> ToolResult {
    let (session, id, tab) = target(handler, ids).await?;
    unless_paused(&session, &id, name, call(&tab.tab)).await
}

/// `evaluate_js`: the result in the shared shape.
pub(crate) async fn evaluate_js(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: EvaluateJsTool = parse_args(params)?;
    let ids = ids(&args.instance_id, &args.tab_id);
    on_page(handler, &ids, "evaluate_js", async |tab| {
        Ok(page::evaluate_js(tab, &args.expression).await?.reply())
    })
    .await
}

/// `inspect_dom`: same text as on Chrome.
pub(crate) async fn inspect_dom(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: InspectDomTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let (before, after) = args.context();
    let expression = crate::util::js::dom_query(args.query.as_deref(), before, after);
    let outcome = page::run_snippet(&tab.tab, &expression).await?;
    let report = crate::util::text::dom_report(&outcome, args.query.as_deref(), before, after);
    Ok(text(report))
}

/// `scroll`: the same JavaScript and reply as on Chrome.
pub(crate) async fn scroll(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: ScrollTool = parse_args(params)?;
    let ids = ids(&args.instance_id, &args.tab_id);
    on_page(handler, &ids, "scroll", async |tab| {
        let outcome = page::run_snippet(tab, &args.expression()).await?;
        args.reply(&outcome)
    })
    .await
}

/// `click_element`: real pointer input at the point the shared snippet
/// checked, with the same checks and replies as on Chrome.
pub(crate) async fn click_element(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: ClickElementTool = parse_args(params)?;
    let ids = ids(&args.instance_id, &args.tab_id);
    let (session, id, tab) = target(handler, &ids).await?;
    let expression = crate::util::js::click_point(&args.selector);
    let checked = page::run_snippet(&tab.tab, &expression);
    let point = unless_paused(&session, &id, "click_element", checked).await?;
    let (x, y) = click_point(&point, &args.selector)?;
    // The click happened even when a dialog or a pause interrupted it.
    let interrupted = match race_pause(&session, &id, page::click_at(&tab.tab, x, y)).await {
        Ok(clicked) => clicked?,
        Err(paused) => Some(paused),
    };
    Ok(clicked_reply(&args.selector, x, y, interrupted.as_ref()))
}

/// `fill_input`: replaces the element's content by typing, with the same
/// checks and replies as on Chrome.
pub(crate) async fn fill_input(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: FillInputTool = parse_args(params)?;
    let ids = ids(&args.instance_id, &args.tab_id);
    on_page(handler, &ids, "fill_input", async |tab| {
        let prepare = crate::util::js::prepare_fill(&args.selector, &args.text);
        let prepared = page::run_snippet(tab, &prepare).await?;
        if let FillStart::Selected = fill_start(&prepared, &args.selector)? {
            return Ok(selected_reply(&args.selector));
        }
        page::type_text(tab, &args.text).await?;
        let check = crate::util::js::fill_result(&args.selector, &args.text);
        fill_reply(&page::run_snippet(tab, &check).await?, &args.selector)
    })
    .await
}

/// `capture_screenshot`: image content.
pub(crate) async fn capture_screenshot(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: CaptureScreenshotTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let format = capture_screenshot::image_format(args.format.as_deref())?;
    let full_page = args.full_page.unwrap_or(false);
    let (clip, note) = if full_page {
        let size = page::run_snippet(&tab.tab, crate::util::js::DOCUMENT_SIZE).await?;
        let side = |key: &str| size[key].as_f64().unwrap_or_default();
        let (width, height) = (side("width"), side("height"));
        match capture_screenshot::full_page_note(width, height) {
            Some(note) => {
                let max = capture_screenshot::FULL_PAGE_MAX;
                (Some((width.min(max), height.min(max))), Some(note))
            }
            None => (None, None),
        }
    } else {
        (None, None)
    };
    let request = page::ShotRequest {
        format,
        quality: args.quality,
        full_page,
        clip,
    };
    let data = page::screenshot(&tab.tab, &request).await?;
    Ok(capture_screenshot::image_reply(data, format, note))
}

// ---- observation ----

/// `get_console_logs`.
pub(crate) async fn console_logs(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: GetConsoleLogsTool = parse_args(params)?;
    let (session, id, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    // Page errors and CSS warnings come from the tab's watcher.
    super::debugger::watch_session_tab(&session, &id, &tab).await;
    Ok(args.report(&tab.log, session.lag_note()).await)
}

/// `get_network_logs`: REST requests (bodies through `network.getData`)
/// and WebSocket frames (from the tab's RDP watcher, see `watch.rs`).
pub(crate) async fn network_logs(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: GetNetworkLogsTool = parse_args(params)?;
    let (session, id, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let watched = super::debugger::watch_session_tab(&session, &id, &tab).await;
    let mut snapshot = args.collect(&tab.network).await;
    let mut notes = Vec::new();
    if args.wants_bodies() {
        notes.extend(get_network_logs::still_loading_note(
            fetch_bodies(&tab.tab, &mut snapshot).await,
        ));
    }
    let wants_frames = args
        .type_filter
        .as_deref()
        .is_none_or(|t| !t.eq_ignore_ascii_case("rest"));
    if wants_frames && !watched {
        notes.push(frames_note(session.spec.lock().await.has_debugger()).to_string());
    }
    notes.extend(session.lag_note());
    Ok(args.finish(snapshot, notes))
}

/// Why a Firefox tab has no WebSocket frames: they come from its debugger.
fn frames_note(has_debugger: bool) -> &'static str {
    if has_debugger {
        "WebSocket frames of this tab are not captured yet: Firefox's debugger could not find the tab (still opening, or closed); the next get_network_logs or debugger tool on it retries"
    } else {
        "WebSocket frames on Firefox need its debugger port (the server runs with --no-firefox-debugger, or the attached Firefox has no --firefox-rdp-port)"
    }
}

/// Response bodies of the requests `get_network_logs::wants_body` picks
/// (spike H11); how many are still loading (Firefox answers
/// `network.getData` for those only once they end).
async fn fetch_bodies(
    tab: &bidi_browser_lite::Tab,
    snapshot: &mut get_network_logs::NetworkSnapshot,
) -> usize {
    let current = &snapshot.current_ids;
    let loading = get_network_logs::still_loading(&snapshot.requests, current);
    for (id, request) in snapshot.requests.iter_mut() {
        if !get_network_logs::wants_body(id, request, current) {
            continue;
        }
        let params = json!({"dataType": "response", "request": id});
        let ask = page::command(tab, "network.getData", params);
        match tokio::time::timeout(get_network_logs::BODY_WAIT, ask).await {
            Ok(Ok(reply)) => {
                let body = reply.pointer("/bytes/value").and_then(Value::as_str);
                request.response_body = body.map(str::to_string);
                let base64 = reply.pointer("/bytes/type").and_then(Value::as_str) == Some("base64");
                request.response_body_encoding = base64.then(|| "base64".to_string());
            }
            // Kind only: the error text could quote the request.
            _ => tracing::debug!(request = %id, "no response body kept by Firefox"),
        }
    }
    loading
}

/// `get_custom_events`: events of modules subscribed with `send_bidi_command`.
pub(crate) async fn custom_events(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: GetCustomEventsTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    Ok(args.report(&tab.custom).await)
}

// ---- tabs ----

/// `open_tab`.
pub(crate) async fn open_tab(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: open_tab::OpenTabTool = parse_args(params)?;
    let url = args.url.clone().unwrap_or_else(|| "about:blank".into());
    check_local(handler, &url)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let browser = session.browser().await?;
    // Checked before creating the page; a page that still fails to
    // register is closed, so none is left open without a tab id.
    read(&session.tabs)
        .check_new(args.label.as_deref())
        .map_err(CallToolError::from_message)?;
    // Created blank and registered before navigating (as on Chrome), so the
    // page's first events reach its buffers.
    let tab = browser
        .new_tab("about:blank")
        .await
        .map_err(super::lifecycle_error)?;
    let context = tab.context_id().to_string();
    let registered = write(&session.tabs).register(tab.clone(), args.label.clone(), &url);
    let tab_id = match registered {
        Ok(id) => id,
        Err(e) => {
            let _ = tab.close().await;
            return Err(CallToolError::from_message(e));
        }
    };
    // Watched before it loads anything, so the first WebSocket frames count.
    if let Ok((_, entry)) = session.tab(Some(&tab_id)).await {
        super::debugger::watch_session_tab(&session, &tab_id, &entry).await;
    }
    let url = match url.as_str() {
        "about:blank" => url,
        _ => {
            page::navigate(&tab, &url)
                .await
                .map_err(|e| {
                    CallToolError::from_message(
                        crate::handler::navigation::open_tab_navigation_failed(
                            &e.to_string(),
                            &tab_id,
                        ),
                    )
                })?
                .url
        }
    };
    let reply = json!({"tab_id": tab_id, "target_id": context, "url": url, "label": args.label});
    Ok(text(reply.to_string()))
}

/// `close_tab`.
pub(crate) async fn close_tab(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: crate::handler::chrome_instance::close_tab::CloseTabTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (id, tab) = session.tab(Some(&args.tab_id)).await?;
    // Forgotten only once Firefox closed it: a tab whose close failed
    // stays reachable.
    tab.tab
        .close()
        .await
        .map_err(|e| super::protocol_error("browsingContext.close", &e))?;
    super::debugger::detach_tab(&session, &id).await;
    write(&session.tabs).remove(&id);
    let active = read(&session.tabs).active.clone();
    Ok(text(
        json!({"closed_tab_id": id, "active_tab_id": active}).to_string(),
    ))
}

/// `list_tabs`.
pub(crate) async fn list_tabs(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    session.browser().await?;
    let tabs = read(&session.tabs);
    let rows: Vec<Value> = tabs
        .tabs
        .iter()
        .map(|(id, t)| json!({"tab_id": id, "label": t.label, "target_id": t.tab.context_id(), "url": t.url}))
        .collect();
    Ok(text(
        json!({"active_tab_id": tabs.active, "tabs": rows}).to_string(),
    ))
}

/// `switch_tab`.
pub(crate) async fn switch_tab(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: switch_tab::SwitchTabTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (id, tab) = session.tab(Some(&args.tab_id)).await?;
    write(&session.tabs).active = Some(id.clone());
    let activate = args.activate.unwrap_or(true);
    if activate {
        tab.tab
            .activate()
            .await
            .map_err(|e| super::protocol_error("browsingContext.activate", &e))?;
    }
    Ok(text(
        json!({"active_tab_id": id, "activated_in_browser": activate}).to_string(),
    ))
}

// ---- instances ----

/// `open_instance` with `browser: "firefox"`.
pub(crate) async fn open_instance(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: open_instance::OpenInstanceTool = parse_args(params)?;
    let id = match &args.label {
        Some(label) => label.clone(),
        None => handler.registry.generate_id(BrowserKind::Firefox),
    };
    let mut spec = handler.firefox_spec.clone();
    spec.browser.headless = args.headless_for(handler);
    spec.browser.user_profile = false;
    spec.firefox.firefox_port = None;
    spec.firefox.firefox_rdp_port = None;
    if let Some(proxy) = &args.proxy {
        spec.proxy = Some(proxy.clone());
    }
    let mut desc = InstanceDescriptor::default_instance(0, Vec::new());
    (desc.id, desc.label, desc.is_default, desc.browser) =
        (id.clone(), args.label.clone(), false, BrowserKind::Firefox);
    handler
        .registry
        .can_add(&desc)
        .map_err(CallToolError::from_message)?;
    let session = Arc::new(FirefoxSession::new(spec));
    session.browser().await?;
    let (host, port, profile_dir) = session.address().await.unwrap_or_default();
    (desc.host, desc.port, desc.profile_dir) = (host, port, profile_dir);
    if let Err(e) = handler.registry.add_firefox(desc.clone(), session.clone()) {
        let _ = session.stop().await;
        return Err(CallToolError::from_message(e));
    }
    let reply = json!({"instance_id": id, "browser": "firefox", "host": desc.host, "port": desc.port, "profile_dir": desc.profile_dir});
    Ok(text(reply.to_string()))
}

/// `close_instance` on a Firefox instance.
pub(crate) async fn close_instance(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: close_instance::CloseInstanceTool = parse_args(params)?;
    let session = handler.firefox(Some(&args.instance_id)).await?;
    let stopped = session.stop().await;
    let is_default = args.instance_id == "default";
    if !is_default {
        handler.registry.remove_session(&args.instance_id);
    }
    stopped?;
    let reply = json!({"instance_id": args.instance_id, "stopped": true, "removed_from_registry": !is_default});
    Ok(text(reply.to_string()))
}

/// `stop_browser` on a Firefox instance (ends its BiDi session).
pub(crate) async fn stop_browser(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    session.stop().await?;
    Ok(text(
        "Firefox instance stopped successfully (its automation session was ended).",
    ))
}

/// `restart_browser` on a Firefox instance.
pub(crate) async fn restart_browser(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: crate::handler::chrome_instance::restart_chrome::RestartChromeTool =
        parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    session.stop().await?;
    if let Some(proxy) = &args.proxy_server {
        session.spec.lock().await.proxy = Some(proxy.clone());
    }
    session.browser().await?;
    if let Some((host, port, profile_dir)) = session.address().await {
        let id = args.instance_id.clone().unwrap_or_else(|| "default".into());
        handler.registry.update_descriptor(&id, |d| {
            (d.host, d.port, d.profile_dir) = (host, port, profile_dir)
        });
    }
    Ok(text("Firefox instance restarted successfully."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> CallToolRequestParams {
        serde_json::from_value(json!({"name": name, "arguments": arguments})).unwrap()
    }

    /// BID-13: --local blocks Firefox navigation like Chrome (before any launch).
    #[tokio::test]
    async fn local_mode_blocks_firefox_navigation() {
        let mut handler = crate::handler::test_handler();
        handler.flags.local = true;
        handler.default_browser = BrowserKind::Firefox;
        let err = navigate(
            call("navigate", json!({"url": "https://example.com"})),
            &handler,
        )
        .await;
        assert!(format!("{:?}", err.unwrap_err()).contains("--local"));
        let err = open_tab(
            call("open_tab", json!({"url": "https://example.com"})),
            &handler,
        )
        .await;
        assert!(format!("{:?}", err.unwrap_err()).contains("--local"));
    }

    use super::super::test_support::{self as ts, evaluate_success};
    use bidi_browser_lite::test_support::MockFirefox;

    /// A Firefox whose navigation `n1` got a document response with
    /// `status` and `headers` (BiDi header list).
    async fn navigation_answered(status: i64, headers: Value) -> (MockFirefox, McpHandler) {
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with(
            "browsingContext.navigate",
            |_| json!({"navigation": "n1", "url": "http://127.0.0.1:8/page"}),
        );
        let handler = ts::handler(&mock);
        handler
            .firefox(None)
            .await
            .unwrap()
            .browser()
            .await
            .unwrap();
        let request = json!({"request": "r1", "url": "http://127.0.0.1:8/page", "method": "GET", "headers": []});
        let sent = json!({"context": "ctx-0", "navigation": "n1", "request": request.clone()});
        mock.bidi().emit("network.beforeRequestSent", sent);
        let response = json!({"status": status, "statusText": "", "headers": headers});
        let done = json!({"context": "ctx-0", "navigation": "n1", "request": request, "response": response});
        mock.bidi().emit("network.responseCompleted", done);
        (mock, handler)
    }

    /// AUD-EV-11 (Firefox): the document's HTTP status is in the reply.
    #[tokio::test]
    async fn navigate_reports_the_http_status() {
        let (_mock, handler) = navigation_answered(404, json!([])).await;
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let reply = navigate(call("navigate", args), &handler).await.unwrap();
        let body = ts::json_of(&reply);
        assert_eq!(body["status"], 404, "{body}");
        assert_eq!(body["notes"][0], "the server answered HTTP 404");
        ts::stop(&handler).await;
    }

    /// AUD-EV-21 (Firefox): a URL Firefox downloads fails, saying so (it
    /// used to answer "Navigated to <previous URL>").
    #[tokio::test]
    async fn download_fails_saying_so() {
        let attachment = json!([{"name": "Content-Disposition", "value": {"type": "string", "value": "attachment; filename=a.bin"}}]);
        let (_mock, handler) = navigation_answered(200, attachment).await;
        let args = json!({"url": "http://127.0.0.1:8/page"});
        let error = navigate(call("navigate", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        let download = crate::handler::navigation::DOWNLOAD;
        assert_eq!(
            error,
            format!("Navigation failed: {download} (http://127.0.0.1:8/page)")
        );
        ts::stop(&handler).await;
    }

    /// BID-12 / NAV-01 / NAV-02: navigate waits for completion and answers
    /// the committed URL (after redirects) in the shared shape.
    #[tokio::test]
    async fn navigate_waits_for_complete() {
        // Given a Firefox instance whose navigation to /start ends on /final
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("browsingContext.getTree", |_| {
            json!({"contexts": [{"context": "ctx-0", "url": "http://127.0.0.1:8/final", "children": null}]})
        });
        let handler = ts::handler(&mock);
        // When navigate runs
        let url = "http://127.0.0.1:8/start";
        let reply = navigate(call("navigate", json!({"url": url})), &handler)
            .await
            .unwrap();
        // Then browsingContext.navigate waited for "complete"
        let sent = mock.bidi().commands_named("browsingContext.navigate");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].params["wait"], "complete");
        assert_eq!(sent[0].params["url"], url);
        // And the reply has the final URL, the tab and the BiDi ids
        assert!(ts::text(&reply).starts_with("Navigated to http://127.0.0.1:8/final."));
        let body = ts::json_of(&reply);
        assert_eq!(body["url"], "http://127.0.0.1:8/final");
        assert_eq!(body["tab_id"], "tab-1");
        assert!(body["browser"]["navigation"].is_string(), "{body}");
        assert_eq!(
            mock.bidi()
                .commands_named("browsingContext.getTree")
                .last()
                .unwrap()
                .params,
            json!({"root": "ctx-0", "maxDepth": 0})
        );
        ts::stop(&handler).await;
    }

    /// Firefox's reply to a navigation to an unknown host (Firefox 157).
    fn unknown_host(mock: &MockFirefox) {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/bidi/navigate-unknown-host.json"
        ))
        .unwrap();
        mock.bidi()
            .respond_with("browsingContext.navigate", move |_| fixture.clone());
    }

    /// NAV-03: a network error fails with the same wording as on Chrome.
    #[tokio::test]
    async fn unknown_host_fails_navigation() {
        // Given a host Firefox cannot resolve
        let mock = MockFirefox::start(false).await.unwrap();
        unknown_host(&mock);
        let handler = ts::handler(&mock);
        // When navigate runs
        let args = json!({"url": "http://nonexistent-host.invalid/"});
        let error = navigate(call("navigate", args), &handler)
            .await
            .unwrap_err();
        // Then it fails with Firefox's code and the URL
        assert_eq!(
            error.to_string(),
            "Navigation failed: NS_ERROR_UNKNOWN_HOST (http://nonexistent-host.invalid/)"
        );
        ts::stop(&handler).await;
    }

    /// NAV-04: open_tab to an unreachable host fails and names the tab,
    /// created blank and registered before the navigation.
    #[tokio::test]
    async fn open_tab_with_bad_url_fails_naming_the_tab() {
        // Given a host Firefox cannot resolve
        let mock = MockFirefox::start(false).await.unwrap();
        unknown_host(&mock);
        let handler = ts::handler(&mock);
        // When open_tab opens it
        let args = json!({"url": "http://nonexistent-host.invalid/"});
        let error = open_tab(call("open_tab", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then the error names the tab that stays open
        assert_eq!(
            error,
            "Navigation failed: NS_ERROR_UNKNOWN_HOST (http://nonexistent-host.invalid/). The tab stays open as tab-2 (close it with close_tab)."
        );
        let created = mock.bidi().commands_named("browsingContext.create");
        assert_eq!(created.len(), 1);
        let navigated = mock.bidi().commands_named("browsingContext.navigate");
        assert_eq!(navigated[0].params["wait"], "complete");
        ts::stop(&handler).await;
    }

    /// NAV-05: reload answers the same shape as navigate.
    #[tokio::test]
    async fn reload_reports_url() {
        // Given a Firefox tab
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with(
            "browsingContext.reload",
            |_| json!({"navigation": "n2", "url": "http://127.0.0.1:8/page"}),
        );
        mock.bidi().respond_with("browsingContext.getTree", |_| {
            json!({"contexts": [{"context": "ctx-0", "url": "http://127.0.0.1:8/page", "children": null}]})
        });
        let handler = ts::handler(&mock);
        // When reload runs
        let reply = reload(call("reload", json!({})), &handler).await.unwrap();
        // Then it reports the URL in the shared shape
        assert!(ts::text(&reply).starts_with("Page reloaded."));
        let body = ts::json_of(&reply);
        assert_eq!(body["url"], "http://127.0.0.1:8/page");
        assert_eq!(body["tab_id"], "tab-1");
        assert_eq!(body["browser"]["navigation"], "n2");
        assert_eq!(
            mock.bidi().commands_named("browsingContext.reload")[0].params["wait"],
            "complete"
        );
        ts::stop(&handler).await;
    }

    /// A mock page whose click snippet answers `point`.
    fn page_with_point(mock: &MockFirefox, point: Value) {
        mock.bidi().respond_with("script.evaluate", move |_| {
            evaluate_success(ts::remote(&point))
        });
    }

    /// A mock page where `#go` is visible at (5, 7).
    fn page_with_button(mock: &MockFirefox) {
        page_with_point(mock, json!({"status": "ok", "x": 5, "y": 7}));
    }

    /// The script and input commands the mock received, in order.
    fn script_and_input(mock: &MockFirefox) -> Vec<String> {
        mock.bidi()
            .commands()
            .into_iter()
            .map(|c| c.method)
            .filter(|m| m.starts_with("script.") || m.starts_with("input."))
            .collect()
    }

    /// BID-16 / AUD-IN-01: click_element clicks, through input actions, the
    /// viewport point the shared snippet checked (scrolled into view, not
    /// covered), so both browsers click the same point.
    #[tokio::test]
    async fn click_uses_the_checked_viewport_point() {
        // Given a page where "#go" is visible at (5, 7)
        let mock = MockFirefox::start(false).await.unwrap();
        page_with_button(&mock);
        let handler = ts::handler(&mock);
        // When click_element "#go" runs
        let reply = click_element(call("click_element", json!({"selector": "#go"})), &handler)
            .await
            .unwrap();
        // Then one check, pointer actions at that viewport point, release
        assert!(ts::text(&reply).contains("(5, 7)"), "{}", ts::text(&reply));
        assert_eq!(
            script_and_input(&mock),
            [
                "script.evaluate",
                "input.performActions",
                "input.releaseActions"
            ]
        );
        let actions = &mock.bidi().commands_named("input.performActions")[0].params;
        let first = &actions["actions"][0]["actions"][0];
        assert_eq!(
            (&first["origin"], &first["x"], &first["y"]),
            (&json!("viewport"), &json!(5), &json!(7))
        );
        ts::stop(&handler).await;
    }

    /// AUD-IN-15: on Firefox too, scroll reports the positions and says when
    /// the window did not move.
    #[tokio::test]
    async fn scroll_reports_when_the_window_did_not_move() {
        let mock = MockFirefox::start(false).await.unwrap();
        let still =
            json!({"status": "done", "before": {"x": 0, "y": 0}, "after": {"x": 0, "y": 0}});
        page_with_point(&mock, still);
        let handler = ts::handler(&mock);
        let reply = scroll(call("scroll", json!({"y": 500})), &handler)
            .await
            .unwrap();
        assert!(ts::text(&reply).starts_with("The window did not move"));
        assert_eq!(ts::json_of(&reply)["after"], json!({"x": 0, "y": 0}));
        ts::stop(&handler).await;
    }

    /// AUD-IN-03: on Firefox too, a covered element is not clicked.
    #[tokio::test]
    async fn click_on_covered_element_sends_no_input() {
        let mock = MockFirefox::start(false).await.unwrap();
        let covered = json!({"status": "covered", "by": "div#overlay", "x": 81, "y": 240});
        page_with_point(&mock, covered);
        let handler = ts::handler(&mock);
        let error = click_element(call("click_element", json!({"selector": "#b"})), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Element '#b' is covered by 'div#overlay'"),
            "{error}"
        );
        assert_eq!(script_and_input(&mock), ["script.evaluate"]);
        ts::stop(&handler).await;
    }

    /// BID-16: a missing element fails and sends no input action.
    #[tokio::test]
    async fn click_on_missing_element_sends_no_input() {
        // Given a page without "#go"
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(json!({"type": "null"}))
        });
        let handler = ts::handler(&mock);
        // When click_element "#go" runs
        let error = click_element(call("click_element", json!({"selector": "#go"})), &handler)
            .await
            .unwrap_err();
        // Then it names the selector and no input action was sent
        assert!(error.to_string().contains("#go"), "{error}");
        assert_eq!(script_and_input(&mock), ["script.evaluate"]);
        ts::stop(&handler).await;
    }

    /// A mock page whose fill preparation answers `prepared` and whose
    /// result snippet answers `typed`.
    fn fill_page(mock: &MockFirefox, prepared: Value, typed: Value) {
        mock.bidi().respond_with("script.evaluate", move |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            let value = if expression.contains("'ready'") {
                &prepared
            } else {
                &typed
            };
            evaluate_success(ts::remote(value))
        });
    }

    /// AUD-EV-05 (Firefox): an open prompt blocks page tools with its text;
    /// handle_dialog answers it through browsingContext.handleUserPrompt.
    #[tokio::test]
    async fn open_prompt_blocks_tools_until_handled() {
        // Given a tab that opened a confirm
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi()
            .respond_with("browsingContext.handleUserPrompt", |_| json!({}));
        let handler = ts::handler(&mock);
        let session = handler.firefox(None).await.unwrap();
        session.browser().await.unwrap();
        let opened =
            json!({"context": "ctx-0", "type": "confirm", "message": "Sure?", "handler": "ignore"});
        mock.bidi().emit("browsingContext.userPromptOpened", opened);
        let (_, tab) = session.tab(None).await.unwrap();
        for _ in 0..100 {
            if tab.dialog.lock().await.open.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // When evaluate_js runs, it fails naming the dialog
        let evaluate = || call("evaluate_js", json!({"expression": "1"}));
        let error = evaluate_js(evaluate(), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("A JavaScript confirm dialog is open on this tab (\"Sure?\")"),
            "{error}"
        );
        // And handle_dialog dismisses it
        let reply = handle_dialog(call("handle_dialog", json!({"accept": false})), &handler)
            .await
            .unwrap();
        assert!(ts::text(&reply).starts_with("Dialog dismissed."));
        let sent = &mock
            .bidi()
            .commands_named("browsingContext.handleUserPrompt")[0]
            .params;
        assert_eq!(sent, &json!({"context": "ctx-0", "accept": false}));
        assert!(tab.dialog.lock().await.open.is_none());
        ts::stop(&handler).await;
    }

    /// AUD-IN-22 (Firefox): a full-page capture of a tall page is clipped
    /// to FULL_PAGE_MAX and the reply says so.
    #[tokio::test]
    async fn tall_full_page_is_cut_with_a_note() {
        use crate::handler::cdp_domains::page::capture_screenshot::FULL_PAGE_MAX;
        // Given a page 1905 x 30000 px
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(ts::remote(&json!({"width": 1905, "height": 30000})))
        });
        mock.bidi().respond_with(
            "browsingContext.captureScreenshot",
            |_| json!({"type": "success", "result": {"data": "iVBO"}}),
        );
        let handler = ts::handler(&mock);
        // When the whole page is captured
        let args = json!({"full_page": true});
        let reply = capture_screenshot(call("capture_screenshot", args), &handler)
            .await
            .unwrap();
        // Then the capture is a box clip of the document's top, with a note
        let sent = &mock
            .bidi()
            .commands_named("browsingContext.captureScreenshot")[0]
            .params;
        assert_eq!(sent["origin"], "document");
        assert_eq!(
            sent["clip"],
            json!({"type": "box", "x": 0, "y": 0, "width": 1905.0, "height": FULL_PAGE_MAX})
        );
        let note = &reply.content[1].as_text_content().unwrap().text;
        assert!(note.contains("16384 of 30000"), "{note}");
        ts::stop(&handler).await;
    }

    /// AUD-EV-12 (Firefox): an object result is held by reference and
    /// measured in the page; one over the limit is described, never sent
    /// whole, and the reference is released.
    #[tokio::test]
    async fn large_objects_are_measured_in_the_page() {
        // Given a page where the expression is an array too large to return
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(json!({"type": "array", "handle": "h1"}))
        });
        mock.bidi().respond_with("script.callFunction", |_| {
            evaluate_success(ts::remote(&json!({"__bcm_too_large__": 25_000_000})))
        });
        let handler = ts::handler(&mock);
        // When evaluate_js runs
        let args = json!({"expression": "big"});
        let reply = evaluate_js(call("evaluate_js", args), &handler)
            .await
            .unwrap();
        // Then the reply describes it
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(body["value"], Value::Null);
        assert_eq!(body["subtype"], "array");
        assert_eq!(body["truncated"], true);
        assert!(
            body["description"].as_str().unwrap().contains("25000000"),
            "{body}"
        );
        // And the evaluation was by reference, the read measured, the handle released
        let sent = &mock.bidi().commands_named("script.evaluate")[0].params;
        assert_eq!(sent["resultOwnership"], "root");
        assert_eq!(sent["serializationOptions"]["maxObjectDepth"], 0);
        let read = &mock.bidi().commands_named("script.callFunction")[0].params;
        assert_eq!(read["this"], json!({"handle": "h1"}));
        assert!(
            read["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("JSON.stringify")
        );
        let released = &mock.bidi().commands_named("script.disown")[0].params;
        assert_eq!(released["handles"], json!(["h1"]));
        ts::stop(&handler).await;
    }

    /// AUD-EV-02 (Firefox): an object is read through MEASURE (its JSON
    /// form, named by its class when not a plain one); one without a JSON
    /// form (a Map) is read as it is.
    #[tokio::test]
    async fn objects_are_read_as_json_and_named() {
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            let kind = if expression == "m" { "map" } else { "object" };
            evaluate_success(json!({"type": kind, "handle": "h1"}))
        });
        mock.bidi().respond_with("script.callFunction", |params| {
            let declaration = params["functionDeclaration"].as_str().unwrap_or_default();
            if declaration.contains("JSON.stringify") {
                evaluate_success(ts::remote(&json!({"__bcm_class__": "URL", "__bcm_value__": "http://a/"})))
            } else {
                evaluate_success(json!({"type": "map", "value": [[{"type": "number", "value": 1}, {"type": "number", "value": 2}]]}))
            }
        });
        let handler = ts::handler(&mock);
        let reply = evaluate_js(call("evaluate_js", json!({"expression": "u"})), &handler)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(
            body,
            json!({"value": "http://a/", "type": "object", "description": "URL"})
        );
        let reply = evaluate_js(call("evaluate_js", json!({"expression": "m"})), &handler)
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&ts::text(&reply)).unwrap();
        assert_eq!(body["description"], "Map(1)", "{body}");
        ts::stop(&handler).await;
    }

    /// AUD-IN-20 (Firefox): a bad format fails before any capture (Firefox
    /// answered a PNG, labelled `image/gif`).
    #[tokio::test]
    async fn bad_format_sends_nothing() {
        let mock = MockFirefox::start(false).await.unwrap();
        let handler = ts::handler(&mock);
        let args = json!({"format": "gif"});
        let error = capture_screenshot(call("capture_screenshot", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unsupported image format 'gif'"), "{error}");
        assert!(
            mock.bidi()
                .commands_named("browsingContext.captureScreenshot")
                .is_empty()
        );
        ts::stop(&handler).await;
    }

    /// AUD-IN-14: a long text is typed in batches of KEYS_PER_ACTION
    /// characters, so no `input.performActions` outlasts the command timeout
    /// (20 000 characters timed out half typed).
    #[tokio::test]
    async fn long_text_is_typed_in_batches() {
        let mock = MockFirefox::start(false).await.unwrap();
        let typed = json!({"status": "done", "matches": true, "empty": false});
        fill_page(&mock, json!({"status": "ready"}), typed);
        let handler = ts::handler(&mock);
        let text = "x".repeat(page::KEYS_PER_ACTION * 2 + 1);
        let args = json!({"selector": "#a", "text": text});
        fill_input(call("fill_input", args), &handler)
            .await
            .unwrap();
        let batches = mock.bidi().commands_named("input.performActions");
        assert_eq!(batches.len(), 3);
        let keys = |i: usize| {
            batches[i].params["actions"][0]["actions"]
                .as_array()
                .unwrap()
                .len()
        };
        assert_eq!(
            (keys(0), keys(1), keys(2)),
            (page::KEYS_PER_ACTION * 2, page::KEYS_PER_ACTION * 2, 2)
        );
        assert_eq!(mock.bidi().commands_named("input.releaseActions").len(), 1);
        ts::stop(&handler).await;
    }

    /// AUD-IN-08: on Firefox too, a non-editable element fails and no key is typed.
    #[tokio::test]
    async fn fill_non_editable_sends_no_keys() {
        // Given a page where "#d" is read-only
        let mock = MockFirefox::start(false).await.unwrap();
        let prepared = json!({"status": "not_editable", "reason": "read-only"});
        fill_page(&mock, prepared, json!(null));
        let handler = ts::handler(&mock);
        // When fill_input targets it
        let args = json!({"selector": "#d", "text": "x"});
        let error = fill_input(call("fill_input", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        // Then it fails like on Chrome and no input action was sent
        assert_eq!(
            error,
            "Element '#d' is not editable (read-only); nothing was typed."
        );
        assert_eq!(script_and_input(&mock), ["script.evaluate"]);
        ts::stop(&handler).await;
    }

    /// AUD-IN-10: on Firefox too, a field left empty after typing is an error.
    #[tokio::test]
    async fn fill_rejected_text_is_an_error() {
        let mock = MockFirefox::start(false).await.unwrap();
        let typed = json!({"status": "done", "matches": false, "empty": true});
        fill_page(&mock, json!({"status": "ready"}), typed);
        let handler = ts::handler(&mock);
        let args = json!({"selector": "#num", "text": "abc"});
        let error = fill_input(call("fill_input", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("'#num' is empty after typing"), "{error}");
        ts::stop(&handler).await;
    }

    /// BID-17: fill_input focuses and clears, then types every character.
    #[tokio::test]
    async fn fill_replaces_value() {
        // Given a page where "#q" exists
        let mock = MockFirefox::start(false).await.unwrap();
        let typed = json!({"status": "done", "matches": true, "empty": false});
        fill_page(&mock, json!({"status": "ready"}), typed);
        let handler = ts::handler(&mock);
        // When fill_input types "ñandú"
        let args = json!({"selector": "#q", "text": "ñandú"});
        fill_input(call("fill_input", args), &handler)
            .await
            .unwrap();
        // Then the clear snippet ran before the key actions, one down/up per char
        let commands = mock.bidi().commands();
        let clear = commands
            .iter()
            .position(|c| c.method == "script.evaluate")
            .unwrap();
        let typed = commands
            .iter()
            .position(|c| c.method == "input.performActions")
            .unwrap();
        assert!(clear < typed);
        assert!(
            commands[clear].params["expression"]
                .as_str()
                .unwrap()
                .contains("focus()")
        );
        let keys = commands[typed].params["actions"][0]["actions"]
            .as_array()
            .unwrap();
        let downs: String = keys
            .iter()
            .filter(|k| k["type"] == "keyDown")
            .map(|k| k["value"].as_str().unwrap())
            .collect();
        assert_eq!(downs, "ñandú");
        assert_eq!(keys.len(), 10);
        assert_eq!(mock.bidi().count("input.releaseActions"), 1);
        ts::stop(&handler).await;
    }

    /// BID-19: inspect_dom works on Firefox with the same output as Chrome.
    #[tokio::test]
    async fn inspect_dom_same_output_as_chrome() {
        use crate::handler::cdp_domains::runtime::inspect_dom::InspectDomTool;
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_mock_chrome_server};
        // Given the HTML the Chrome mock serves, also served by the Firefox mock
        let html = "<html><body><h1>Hello World</h1><div id='test'>This is a test</div><input id='test-input' value='' /></body></html>";
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", move |_| {
            evaluate_success(json!({"type": "string", "value": html}))
        });
        let firefox = ts::handler(&mock);
        let chrome = mock_handler(spawn_mock_chrome_server().await);
        // When inspect_dom looks for "test" on both
        let args = json!({"query": "test", "before": 10, "after": 10});
        let on_firefox = inspect_dom(call("inspect_dom", args.clone()), &firefox)
            .await
            .unwrap();
        let on_chrome = InspectDomTool::handle(call("inspect_dom", args), &chrome)
            .await
            .unwrap();
        // Then the replies are identical
        assert_eq!(ts::text(&on_firefox), ts::text(&on_chrome));
        ts::stop(&firefox).await;
    }

    /// The `get_network_logs` reply (and the mock) for one answered request
    /// of the current navigation whose `network.getData` answers `bytes`.
    async fn body_reply(bytes: Value) -> (MockFirefox, CallToolResult) {
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi()
            .respond_with("network.getData", move |_| json!({"bytes": bytes.clone()}));
        let handler = ts::handler(&mock);
        let session = handler.firefox(None).await.unwrap();
        let (_, tab) = session.tab(None).await.unwrap();
        {
            let mut network = tab.network.lock().await;
            for (method, fixture) in [
                (
                    "network.beforeRequestSent",
                    include_str!("../../../tests/fixtures/bidi/event-before-request-sent.json"),
                ),
                (
                    "network.responseCompleted",
                    include_str!("../../../tests/fixtures/bidi/event-response-completed.json"),
                ),
            ] {
                let event: Value = serde_json::from_str(fixture).unwrap();
                super::super::pump::on_network(method, &event["params"], &mut network);
            }
        }
        let args = json!({"include_details": true, "type_filter": "rest"});
        let reply = network_logs(call("get_network_logs", args), &handler)
            .await
            .unwrap();
        ts::stop(&handler).await;
        (mock, reply)
    }

    /// BID-22: response bodies are fetched with network.getData (spike H11 ✅).
    #[tokio::test]
    async fn response_bodies_follow_spike() {
        // Given one answered request of the current navigation on tab-1
        // When get_network_logs asks for details
        let (mock, reply) = body_reply(json!({"type": "string", "value": "{\"ok\":true}"})).await;
        // Then the body came from network.getData for that request
        let sent = mock.bidi().commands_named("network.getData");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].params["dataType"], "response");
        assert!(sent[0].params["request"].is_string());
        assert!(
            ts::text(&reply).contains("{\\\"ok\\\":true}"),
            "{}",
            ts::text(&reply)
        );
        assert!(!ts::text(&reply).contains("response_body_encoding"));
    }

    /// AUD-OB-14 (Firefox): a response still loading (only
    /// responseStarted) is not asked for its body, and the reply says so
    /// (network.getData waited 10 s for an open event stream).
    #[tokio::test]
    async fn unfinished_responses_are_noted_not_fetched() {
        let mock = MockFirefox::start(false).await.unwrap();
        let handler = ts::handler(&mock);
        let session = handler.firefox(None).await.unwrap();
        let (_, tab) = session.tab(None).await.unwrap();
        {
            let mut network = tab.network.lock().await;
            for (method, fixture) in [
                (
                    "network.beforeRequestSent",
                    include_str!("../../../tests/fixtures/bidi/event-before-request-sent.json"),
                ),
                (
                    "network.responseStarted",
                    include_str!("../../../tests/fixtures/bidi/event-response-completed.json"),
                ),
            ] {
                let event: Value = serde_json::from_str(fixture).unwrap();
                super::super::pump::on_network(method, &event["params"], &mut network);
            }
        }
        let args = json!({"include_details": true, "type_filter": "rest"});
        let reply = network_logs(call("get_network_logs", args), &handler)
            .await
            .unwrap();
        assert!(mock.bidi().commands_named("network.getData").is_empty());
        let notes = &ts::json_of(&reply)["notes"];
        assert!(
            notes[0]
                .as_str()
                .unwrap()
                .starts_with("1 response still loading"),
            "{notes}"
        );
        ts::stop(&handler).await;
    }

    /// AUD-OB-15 (Firefox): a binary body says it is base64.
    #[tokio::test]
    async fn base64_bodies_are_marked() {
        let (_mock, reply) = body_reply(json!({"type": "base64", "value": "iVBORw0K"})).await;
        let body = ts::json_of(&reply);
        let request = body["requests"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(request["response_body"], "iVBORw0K");
        assert_eq!(request["response_body_encoding"], "base64");
    }

    /// The resources of a Firefox 157 fixture packet.
    fn fixture_resources(fixture: &str) -> Vec<Value> {
        let packet: Value = serde_json::from_str(fixture).unwrap();
        packet["array"][0][1].as_array().unwrap().clone()
    }

    /// Calls `name` until its reply contains `needle` (the watcher's
    /// resources arrive asynchronously); the last reply.
    async fn until_contains(handler: &McpHandler, name: &str, args: Value, needle: &str) -> String {
        let mut reply = String::new();
        for _ in 0..100 {
            let result = match name {
                "get_network_logs" => network_logs(call(name, args.clone()), handler).await,
                _ => console_logs(call(name, args.clone()), handler).await,
            };
            reply = ts::text(&result.unwrap());
            if reply.contains(needle) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        reply
    }

    /// FWS-01 / FWS-02 / FWS-07: frames are recorded both ways, the filters
    /// work, and frames keep flowing after a navigation.
    #[tokio::test]
    async fn firefox_websocket_frames_are_captured() {
        use bidi_browser_lite::test_support::fixtures::RDP_RESOURCES_WEBSOCKET;
        // Given a Firefox tab watched since the session started
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        handler
            .firefox(None)
            .await
            .unwrap()
            .browser()
            .await
            .unwrap();
        // When the page exchanges frames
        rdp.emit_resources("websocket", fixture_resources(RDP_RESOURCES_WEBSOCKET));
        // Then get_network_logs filters them like on Chrome
        let args =
            json!({"type_filter": "websocket", "ws_content_filter": "echo", "url_filter": "8808"});
        let reply = until_contains(&handler, "get_network_logs", args, "echo:").await;
        let body: Value = serde_json::from_str(&reply).unwrap();
        let frames = body["websocket_frames"]["38"].as_array().unwrap();
        assert_eq!(frames.len(), 1, "{reply}");
        assert!(body.get("notes").is_none(), "{reply}");
        // And after a navigation (new target) frames keep coming
        rdp.navigate();
        let mut later = fixture_resources(RDP_RESOURCES_WEBSOCKET);
        later[0]["httpChannelId"] = json!(41);
        later[1]["httpChannelId"] = json!(41);
        rdp.emit_resources("websocket", later[..2].to_vec());
        let args = json!({"type_filter": "websocket"});
        let reply = until_contains(&handler, "get_network_logs", args, "\"41\"").await;
        assert!(reply.contains("\"41\""), "{reply}");
        ts::stop(&handler).await;
    }

    /// FWS-03 / FWS-04: page errors reach the console with honest labels,
    /// once even when BiDi reports them too.
    #[tokio::test]
    async fn firefox_page_errors_reach_the_console_once() {
        use bidi_browser_lite::test_support::fixtures::RDP_RESOURCES_ERROR_MESSAGE;
        // Given a watched Firefox tab
        let mock = ts::firefox_with_debugger().await;
        let rdp = mock.rdp().unwrap();
        let handler = ts::handler(&mock);
        handler
            .firefox(None)
            .await
            .unwrap()
            .browser()
            .await
            .unwrap();
        // When BiDi and the watcher both report one script error
        let text = "ReferenceError: nonExistentFunctionCallHere is not defined";
        let bidi = json!({"type": "javascript", "level": "error", "text": text,
            "timestamp": 1_791_822_789_737_u64, "source": {"context": "ctx-0", "realm": "r"}});
        mock.bidi().emit("log.entryAdded", bidi);
        let errors = fixture_resources(RDP_RESOURCES_ERROR_MESSAGE);
        rdp.emit_resources("error-message", errors);
        // Then it is listed once, as a javascript error, next to the cookie warning
        let reply = until_contains(&handler, "get_console_logs", json!({}), "security").await;
        let body: Value = serde_json::from_str(&reply).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let script: Vec<&Value> = messages.iter().filter(|m| m["text"] == text).collect();
        assert_eq!(script.len(), 1, "{reply}");
        assert_eq!(script[0]["source"], "javascript");
        assert!(
            messages.iter().any(|m| m["source"] == "security"),
            "{reply}"
        );
        ts::stop(&handler).await;
    }

    /// BKD-06: Tab ids are MCP-level and map to native ids.
    #[tokio::test]
    async fn tab_ids_map_to_native_ids() {
        // Given a Firefox instance over the mock (tab-1 is its first context)
        let mock = MockFirefox::start(false).await.unwrap();
        let handler = ts::handler(&mock);
        // When open_tab creates a page
        let opened = ts::json_of(
            &open_tab(call("open_tab", json!({})), &handler)
                .await
                .unwrap(),
        );
        // Then the reply has an MCP tab id and the native context id
        assert_eq!(opened["tab_id"], "tab-2");
        let context = opened["target_id"].as_str().unwrap().to_string();
        // And a later call with that tab id reaches the native context
        let args = json!({"tab_id": "tab-2", "url": "http://127.0.0.1:8/"});
        navigate(call("navigate", args), &handler).await.unwrap();
        let sent = mock.bidi().commands_named("browsingContext.navigate");
        assert_eq!(sent.last().unwrap().params["context"], context.as_str());
        ts::stop(&handler).await;
    }

    /// BID-10: Lost events are reported, not hidden.
    #[test]
    fn lagged_events_are_reported() {
        let session = FirefoxSession::new(Default::default());
        session
            .lagged
            .store(7, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            session.lag_note().as_deref(),
            Some("7 browser events were lost (the server fell behind)")
        );
        assert_eq!(session.lag_note(), None, "reported once");
    }
}
