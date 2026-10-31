//! `mock_route`, `list_routes`, `remove_route` and `set_offline` on Firefox:
//! a `beforeRequestSent` intercept on the tab blocks its requests while it
//! has routes; a blocked request gets `network.provideResponse` from the
//! latest matching route, or `network.continueRequest`. Offline is
//! `emulation.setNetworkConditions`.

use super::page;
use super::session::FirefoxTab;
use super::tools::{ids, target};
use crate::handler::cdp_domains::fetch::mock_route::{
    ListRoutesTool, MockRouteTool, RemoveRouteTool, listed, removed,
};
use crate::handler::cdp_domains::network::set_offline::{self, SetOfflineTool};
use crate::handler::routes::{MockResponse, RouteState};
use crate::handler::{McpHandler, parse_args};
use bidi_browser_lite::{BidiClient, WsResponse};
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;

type ToolResult = Result<CallToolResult, CallToolError>;

/// `mock_route`.
pub(crate) async fn mock_route(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: MockRouteTool = parse_args(params)?;
    let response = args.response()?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let added = add_route(&tab, &args.url_pattern, response.clone()).await;
    let id = added.map_err(CallToolError::from_message)?;
    Ok(args.reply(&id, &response))
}

/// `list_routes`.
pub(crate) async fn list_routes(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: ListRoutesTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    Ok(listed(&*tab.routes.lock().await))
}

/// `remove_route`: the last route removes the intercept.
pub(crate) async fn remove_route(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> ToolResult {
    let args: RemoveRouteTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let mut routes = tab.routes.lock().await;
    let ids = routes
        .remove(args.route_id.as_deref())
        .map_err(CallToolError::from_message)?;
    if routes.is_empty() {
        stop_intercepting(&tab, &mut routes).await;
    }
    Ok(removed(&ids, &routes))
}

/// `set_offline`.
pub(crate) async fn set_offline(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: SetOfflineTool = parse_args(params)?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let conditions = args.offline.then(|| json!({"type": "offline"}));
    let params = json!({"networkConditions": conditions, "contexts": [tab.tab.context_id()]});
    page::command(&tab.tab, "emulation.setNetworkConditions", params).await?;
    let online = page::run_snippet(&tab.tab, set_offline::MEASURE).await?;
    Ok(args.reply(&online))
}

/// Adds a route to the tab's table; the first one adds the intercept and
/// starts the task answering the requests it blocks. Errors are text: the
/// tool error type is not `Send`.
async fn add_route(
    tab: &FirefoxTab,
    url_pattern: &str,
    response: MockResponse,
) -> Result<String, String> {
    let mut routes = tab.routes.lock().await;
    let id = routes.add(url_pattern, response)?;
    if routes.intercept.is_some() {
        return Ok(id);
    }
    let client = tab.tab.client().clone();
    // Subscribed before the intercept: a request blocked in between would
    // otherwise wait for nobody.
    let events = client.subscribe();
    let added = page::command(&tab.tab, "network.addIntercept", intercept_params(tab)).await;
    let intercept = added
        .ok()
        .and_then(|reply| reply.get("intercept")?.as_str().map(str::to_string));
    let Some(intercept) = intercept else {
        let _ = routes.remove(Some(&id));
        return Err("Firefox refused network.addIntercept".into());
    };
    let answering = answer_requests(
        client,
        events,
        intercept.clone(),
        Arc::downgrade(&tab.routes),
    );
    routes.listener = Default::default();
    routes.listener.push(tokio::spawn(answering));
    routes.intercept = Some(intercept);
    Ok(id)
}

/// The tab's intercept: every request of the tab (and its iframes) is
/// blocked; the routes' globs are matched here, as BiDi patterns are not
/// globs.
fn intercept_params(tab: &FirefoxTab) -> Value {
    json!({"phases": ["beforeRequestSent"], "contexts": [tab.tab.context_id()]})
}

/// Removes the tab's intercept and stops answering.
async fn stop_intercepting(tab: &FirefoxTab, routes: &mut RouteState) {
    if let Some(intercept) = routes.intercept.take() {
        let params = json!({"intercept": intercept});
        let _ = page::command(&tab.tab, "network.removeIntercept", params).await;
    }
    routes.listener = Default::default();
}

/// Answers the requests the intercept `intercept` blocks: a response from
/// the latest matching route, or let through. Holds the routes weakly:
/// they own this task.
async fn answer_requests(
    client: BidiClient,
    mut events: tokio::sync::broadcast::Receiver<WsResponse>,
    intercept: String,
    routes: Weak<Mutex<RouteState>>,
) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(skipped)) => {
                // Their requests stay blocked until Firefox times them out.
                tracing::warn!(skipped, "Firefox mocked routes lagged; requests may stall");
                continue;
            }
            Err(RecvError::Closed) => break,
        };
        let Some((request, url)) = blocked_request(&event, &intercept) else {
            continue;
        };
        let Some(routes) = routes.upgrade() else {
            break;
        };
        let answer = routes.lock().await.answer(&url);
        let (method, params) = match answer {
            Some(response) => ("network.provideResponse", provide(&request, &response)),
            None => ("network.continueRequest", json!({"request": request})),
        };
        if client.send_raw_command(method, params).await.is_err() {
            tracing::warn!(method, "Firefox mocked route answer failed");
        }
    }
}

/// The request id and URL of a `network.beforeRequestSent` event that
/// `intercept` blocked.
fn blocked_request(event: &WsResponse, intercept: &str) -> Option<(String, String)> {
    if event.method.as_deref() != Some("network.beforeRequestSent") {
        return None;
    }
    let params = event.params.as_ref()?;
    let ours = params
        .get("intercepts")?
        .as_array()?
        .iter()
        .any(|i| i == intercept);
    if params.get("isBlocked") != Some(&Value::Bool(true)) || !ours {
        return None;
    }
    let request = params.pointer("/request/request")?.as_str()?.to_string();
    let url = params.pointer("/request/url")?.as_str()?.to_string();
    Some((request, url))
}

/// `network.provideResponse`'s params for `response`.
fn provide(request: &str, response: &MockResponse) -> Value {
    let headers: Vec<Value> = response
        .headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": {"type": "string", "value": value}}))
        .collect();
    json!({"request": request, "statusCode": response.status, "headers": headers,
           "body": {"type": "string", "value": response.body}})
}

#[cfg(test)]
mod tests {
    use super::super::test_support as ts;
    use super::*;

    /// NET-05 (Firefox): the first route adds a beforeRequestSent
    /// intercept on the tab; a request it blocks gets the route's response,
    /// any other one is continued; the last route removes the intercept.
    #[tokio::test]
    async fn firefox_answers_blocked_requests_from_routes() {
        // Given a tab with a route for */api/*
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        mock.bidi()
            .respond_with("network.addIntercept", |_| json!({"intercept": "i1"}));
        let handler = ts::handler(&mock);
        let args = json!({"url_pattern": "*/api/*", "status": 201, "body": "{\"ok\":1}",
            "content_type": "application/json"});
        let reply = mock_route(ts::call("mock_route", args), &handler)
            .await
            .unwrap();
        assert!(ts::text(&reply).contains("Route route-1 added"));
        let added = &mock.bidi().commands_named("network.addIntercept")[0].params;
        assert_eq!(added["phases"], json!(["beforeRequestSent"]));
        // When Firefox blocks a matching request, and another one
        let blocked = |id: &str, url: &str| {
            json!({"context": "ctx-0", "isBlocked": true, "intercepts": ["i1"], "navigation": null,
                "redirectCount": 0, "timestamp": 0,
                "request": {"request": id, "url": url, "method": "GET", "headers": [], "cookies": [],
                    "headersSize": 0, "bodySize": 0, "timings": {}}})
        };
        mock.bidi()
            .emit("network.beforeRequestSent", blocked("R1", "http://a/api/x"));
        mock.bidi()
            .emit("network.beforeRequestSent", blocked("R2", "http://a/page"));
        for _ in 0..100 {
            if mock.bidi().count("network.continueRequest") > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Then the first gets the route's response, the second goes through
        let provided = &mock.bidi().commands_named("network.provideResponse")[0].params;
        assert_eq!(
            *provided,
            json!({"request": "R1", "statusCode": 201,
                "headers": [{"name": "Content-Type", "value": {"type": "string", "value": "application/json"}}],
                "body": {"type": "string", "value": "{\"ok\":1}"}})
        );
        let continued = &mock.bidi().commands_named("network.continueRequest")[0].params;
        assert_eq!(*continued, json!({"request": "R2"}));
        // When the last route is removed, the intercept goes
        let reply = remove_route(
            ts::call("remove_route", json!({"route_id": "route-1"})),
            &handler,
        )
        .await
        .unwrap();
        assert!(ts::text(&reply).contains("0 left"));
        let removed = &mock.bidi().commands_named("network.removeIntercept")[0].params;
        assert_eq!(*removed, json!({"intercept": "i1"}));
        ts::stop(&handler).await;
    }

    /// NET-07 (Firefox): set_offline sets the tab's network conditions,
    /// and null puts it back online.
    #[tokio::test]
    async fn firefox_goes_offline_and_back() {
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        let handler = ts::handler(&mock);
        for offline in [true, false] {
            let args = json!({"offline": offline});
            set_offline(ts::call("set_offline", args), &handler)
                .await
                .unwrap();
        }
        let sent = mock.bidi().commands_named("emulation.setNetworkConditions");
        assert_eq!(
            sent[0].params["networkConditions"],
            json!({"type": "offline"})
        );
        assert_eq!(sent[1].params["networkConditions"], Value::Null);
        assert_eq!(sent[0].params["contexts"], json!(["ctx-0"]));
        ts::stop(&handler).await;
    }
}
