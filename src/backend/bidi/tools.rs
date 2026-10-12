//! The Firefox implementation of the tools. Each function parses the same
//! arguments as the Chrome tool (one schema per tool) and answers in the same
//! shape where the protocols allow it.

use super::page;
use super::session::FirefoxSession;
use crate::backend::BrowserKind;
use crate::handler::cdp_domains::custom::get_custom_events::GetCustomEventsTool;
use crate::handler::cdp_domains::input::{
    click_element::ClickElementTool, fill_input::FillInputTool,
};
use crate::handler::cdp_domains::log::get_console_logs::GetConsoleLogsTool;
use crate::handler::cdp_domains::network::get_network_logs::GetNetworkLogsTool;
use crate::handler::cdp_domains::page::capture_screenshot::CaptureScreenshotTool;
use crate::handler::cdp_domains::page::navigate::NavigateTool;
use crate::handler::cdp_domains::page::scroll::ScrollTool;
use crate::handler::cdp_domains::runtime::evaluate_js::EvaluateJsTool;
use crate::handler::cdp_domains::runtime::inspect_dom::InspectDomTool;
use crate::handler::chrome_instance::registry::InstanceDescriptor;
use crate::handler::chrome_instance::{close_instance, open_instance, open_tab, switch_tab};
use crate::handler::{McpHandler, TargetArgs, parse_args};
use crate::util::sync::{read, write};
use rust_mcp_sdk::schema::{
    CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, ImageContent,
};
use serde_json::{Value, json};
use std::sync::Arc;

type ToolResult = Result<CallToolResult, CallToolError>;

fn text(value: impl Into<String>) -> CallToolResult {
    CallToolResult::text_content(vec![value.into().into()])
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// The session and tab a call addresses.
async fn target(
    handler: &McpHandler,
    ids: &TargetArgs,
) -> Result<(Arc<FirefoxSession>, String, super::session::FirefoxTab), CallToolError> {
    let session = handler.firefox(ids.instance_id.as_deref()).await?;
    let (id, tab) = session.tab(ids.tab_id.as_deref()).await?;
    Ok((session, id, tab))
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
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let reply = page::navigate(&tab.tab, &args.url).await?;
    Ok(text(format!(
        "Navigated to {}.\n{}",
        args.url,
        pretty(&reply)
    )))
}

/// `reload`.
pub(crate) async fn reload(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let ids: TargetArgs = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids).await?;
    let reply = page::reload(&tab.tab).await?;
    Ok(text(format!("Page reloaded.\n{}", pretty(&reply))))
}

/// `evaluate_js`: the value as plain JSON.
pub(crate) async fn evaluate_js(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: EvaluateJsTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let value = page::evaluate(&tab.tab, &args.expression).await?;
    Ok(text(pretty(&value)))
}

/// `inspect_dom`: same text as on Chrome.
pub(crate) async fn inspect_dom(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: InspectDomTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let html = page::evaluate(&tab.tab, "document.documentElement.outerHTML").await?;
    let report = crate::util::text::dom_query_report(
        html.as_str().unwrap_or_default(),
        args.query.as_deref(),
        args.before.unwrap_or(200) as usize,
        args.after.unwrap_or(200) as usize,
    );
    Ok(text(report))
}

/// `scroll`: the same JavaScript as on Chrome.
pub(crate) async fn scroll(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: ScrollTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let value = page::run_snippet(&tab.tab, &args.expression()).await?;
    if let Some(selector) = &args.selector
        && value != Value::Bool(true)
    {
        return Err(CallToolError::from_message(format!(
            "Element not found for scrolling: {selector}"
        )));
    }
    Ok(text("Successfully scrolled."))
}

/// `click_element`: real pointer input on the element.
pub(crate) async fn click_element(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: ClickElementTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let (x, y) = page::click(&tab.tab, &args.selector).await?;
    Ok(text(format!(
        "Successfully clicked on element '{}' at coordinates ({x}, {y})",
        args.selector
    )))
}

/// `fill_input`: replaces the element's content by typing.
pub(crate) async fn fill_input(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: FillInputTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    page::fill(&tab.tab, &args.selector, &args.text).await?;
    Ok(text(format!(
        "Successfully filled input '{}' with text.",
        args.selector
    )))
}

/// `capture_screenshot`: image content.
pub(crate) async fn capture_screenshot(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: CaptureScreenshotTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let format = args.format.clone().unwrap_or_else(|| "png".into());
    let request = page::ShotRequest {
        format: &format,
        quality: args.quality,
        full_page: args.full_page.unwrap_or(false),
    };
    let data = page::screenshot(&tab.tab, &request).await?;
    let image = ImageContent::new(data, format!("image/{format}"), None, None);
    Ok(CallToolResult {
        content: vec![ContentBlock::ImageContent(image)],
        is_error: None,
        meta: None,
        structured_content: None,
    })
}

// ---- observation ----

/// `get_console_logs`.
pub(crate) async fn console_logs(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: GetConsoleLogsTool = parse_args(params)?;
    let (session, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    Ok(args.report(&tab.log, session.lag_note()).await)
}

/// `get_network_logs`: REST requests (bodies through `network.getData`);
/// WebSocket frames do not exist in BiDi.
pub(crate) async fn network_logs(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: GetNetworkLogsTool = parse_args(params)?;
    let (session, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let mut snapshot = args.collect(&tab.network).await;
    if args.wants_bodies() {
        fetch_bodies(&tab.tab, &mut snapshot).await;
    }
    let mut notes = Vec::new();
    if args
        .type_filter
        .as_deref()
        .is_none_or(|t| t.eq_ignore_ascii_case("both"))
    {
        notes.push("WebSocket frames are not available on Firefox (BiDi)".to_string());
    }
    notes.extend(session.lag_note());
    Ok(args.finish(snapshot, notes))
}

/// Response bodies of answered requests of the current navigation (spike H11).
async fn fetch_bodies(
    tab: &bidi_browser_lite::Tab,
    snapshot: &mut crate::handler::cdp_domains::network::get_network_logs::NetworkSnapshot,
) {
    for (id, request) in snapshot.requests.iter_mut() {
        if !snapshot.current_ids.contains(id) || request.response_status.is_none() {
            continue;
        }
        let params = json!({"dataType": "response", "request": id});
        match page::command(tab, "network.getData", params).await {
            Ok(reply) => {
                let body = reply.pointer("/bytes/value").and_then(Value::as_str);
                request.response_body = body.map(str::to_string);
            }
            // Kind only: the error text could quote the request.
            Err(_) => tracing::debug!(request = %id, "no response body kept by Firefox"),
        }
    }
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
    let tab = browser
        .new_tab(&url)
        .await
        .map_err(super::lifecycle_error)?;
    let context = tab.context_id().to_string();
    let tab_id = write(&session.tabs)
        .register(tab, args.label.clone(), &url)
        .map_err(CallToolError::from_message)?;
    let reply = json!({"tab_id": tab_id, "target_id": context, "url": url, "label": args.label});
    Ok(text(reply.to_string()))
}

/// `close_tab`.
pub(crate) async fn close_tab(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: crate::handler::chrome_instance::close_tab::CloseTabTool = parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    let (id, tab) = session.tab(Some(&args.tab_id)).await?;
    super::debugger::detach_tab(&session, &id).await;
    write(&session.tabs).remove(&id);
    tab.tab
        .close()
        .await
        .map_err(|e| super::protocol_error("browsingContext.close", &e))?;
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
    spec.browser.headless = args.headless.unwrap_or(false);
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
