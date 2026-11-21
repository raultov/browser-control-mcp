//! `mock_route`, `list_routes`, `remove_route` and `set_offline` on Firefox:
//! a `beforeRequestSent` intercept on the tab blocks its requests while it
//! has routes; a blocked request gets `network.provideResponse` from the
//! latest matching route, or `network.continueRequest`. While a route is
//! upstream (#193), a `responseStarted` intercept blocks the tab's
//! responses too (BiDi patterns are not globs: every response); a blocked
//! response gets `network.continueResponse` with the latest matching
//! route's changes, or as it is. Offline is
//! `emulation.setNetworkConditions`, which fails the mocked requests too:
//! Firefox accepts the `provideResponse`, then the page gets a network
//! error (#108). The tools say so instead of answering as on Chrome.

use super::page;
use super::session::FirefoxTab;
use super::tools::{ids, target};
use crate::handler::cdp_domains::fetch::mock_route::{
    ListRoutesTool, MockRouteTool, RemoveRouteTool, listed, removed,
};
use crate::handler::cdp_domains::network::set_offline::{self, SetOfflineTool};
use crate::handler::routes::{
    MAX_PROVIDED, MockResponse, ResponseOverride, RouteAction, RouteState, is_redirect,
    reason_phrase,
};
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
    let action = args.action()?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let added = add_route(&tab, &args.url_pattern, action.clone()).await;
    let id = added.map_err(CallToolError::from_message)?;
    let offline = tab.routes.lock().await.offline;
    Ok(noted(args.reply(&id, &action), offline))
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
    } else if !routes.has_upstream() {
        remove_intercept(&tab, routes.response_intercept.take()).await;
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
    let routed = {
        let mut routes = tab.routes.lock().await;
        routes.offline = args.offline;
        !routes.is_empty()
    };
    let online = page::run_snippet(&tab.tab, set_offline::MEASURE).await?;
    Ok(noted(args.reply(&online), args.offline && routed))
}

/// What Firefox does to mocked routes while the tab is offline (#108).
const OFFLINE_ROUTES: &str = "Note: on Firefox the tab's mocked routes do not answer while it is offline (its offline emulation fails their requests too, unlike Chrome): call set_offline with offline false to use them.";

/// `reply` with [`OFFLINE_ROUTES`] as its second line when
/// `offline_with_routes`: after the first line, before the reply object
/// (as its first block when the reply starts with no text).
fn noted(mut reply: CallToolResult, offline_with_routes: bool) -> CallToolResult {
    if !offline_with_routes {
        return reply;
    }
    match reply.content.first_mut() {
        Some(rust_mcp_sdk::schema::ContentBlock::TextContent(first)) => {
            first.text = match first.text.split_once('\n') {
                Some((line, body)) => format!("{line}\n{OFFLINE_ROUTES}\n{body}"),
                None => format!("{}\n{OFFLINE_ROUTES}", first.text),
            };
        }
        _ => {
            let note =
                rust_mcp_sdk::schema::ContentBlock::TextContent(OFFLINE_ROUTES.to_string().into());
            reply.content.insert(0, note);
        }
    }
    reply
}

/// Adds a route to the tab's table; the first one adds the request
/// intercept and starts the task answering what it blocks, the first
/// upstream one the response intercept. Errors are text: the tool error
/// type is not `Send`.
async fn add_route(
    tab: &FirefoxTab,
    url_pattern: &str,
    action: RouteAction,
) -> Result<String, String> {
    let upstream = matches!(action, RouteAction::Upstream(_));
    let mut routes = tab.routes.lock().await;
    let id = routes.add(url_pattern, action)?;
    if routes.intercept.is_none() {
        let client = tab.tab.client().clone();
        // Subscribed before the intercept: a request blocked in between
        // would otherwise wait for nobody.
        let events = client.subscribe();
        let Some(intercept) = add_intercept(tab, "beforeRequestSent").await else {
            let _ = routes.remove(Some(&id));
            return Err("Firefox refused network.addIntercept".into());
        };
        let answering = answer_requests(client, events, Arc::downgrade(&tab.routes));
        routes.listener = Default::default();
        routes.listener.push(tokio::spawn(answering));
        routes.intercept = Some(intercept);
    }
    if upstream && routes.response_intercept.is_none() {
        let Some(intercept) = add_intercept(tab, "responseStarted").await else {
            let _ = routes.remove(Some(&id));
            if routes.is_empty() {
                stop_intercepting(tab, &mut routes).await;
            }
            return Err("Firefox refused network.addIntercept for responses".into());
        };
        routes.response_intercept = Some(intercept);
    }
    Ok(id)
}

/// Adds an intercept of `phase` on the tab: every request of the tab (and
/// its iframes) is blocked at it; the routes' globs are matched here, as
/// BiDi patterns are not globs. Its id.
async fn add_intercept(tab: &FirefoxTab, phase: &str) -> Option<String> {
    let params = json!({"phases": [phase], "contexts": [tab.tab.context_id()]});
    let added = page::command(&tab.tab, "network.addIntercept", params).await;
    added
        .ok()
        .and_then(|reply| reply.get("intercept")?.as_str().map(str::to_string))
}

/// Removes the intercept `intercept`, if any.
async fn remove_intercept(tab: &FirefoxTab, intercept: Option<String>) {
    if let Some(intercept) = intercept {
        let params = json!({"intercept": intercept});
        let _ = page::command(&tab.tab, "network.removeIntercept", params).await;
    }
}

/// Removes the tab's intercepts and stops answering.
async fn stop_intercepting(tab: &FirefoxTab, routes: &mut RouteState) {
    remove_intercept(tab, routes.intercept.take()).await;
    remove_intercept(tab, routes.response_intercept.take()).await;
    routes.listener = Default::default();
}

/// Answers what the tab's intercepts block: a request gets a response
/// from the latest matching route, or is let through; a response (while a
/// route is upstream) gets the latest matching route's changes, or is let
/// through as it is. Holds the routes weakly: they own this task.
async fn answer_requests(
    client: BidiClient,
    mut events: tokio::sync::broadcast::Receiver<WsResponse>,
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
        let Some(blocked) = blocked(&event) else {
            continue;
        };
        // The routes live as long as the tab: once they are gone its
        // context is closed or its session ended, and nothing of it is
        // still blocked.
        let Some(routes) = routes.upgrade() else {
            break;
        };
        let Some((method, params)) = answered(&blocked, &mut *routes.lock().await) else {
            continue;
        };
        if client.send_raw_command(method, params).await.is_err() {
            tracing::warn!(method, "Firefox mocked route answer failed");
        }
    }
}

/// A request or a response an intercept blocked.
struct Blocked<'a> {
    request: String,
    url: String,
    /// The intercepts that blocked it.
    intercepts: &'a [Value],
    /// A response's headers (`network.responseStarted`); `None` for a
    /// request (`network.beforeRequestSent`).
    response_headers: Option<&'a [Value]>,
    /// A response's status.
    status: u64,
}

/// What a blocked `network.beforeRequestSent` or `network.responseStarted`
/// event blocked.
fn blocked(event: &WsResponse) -> Option<Blocked<'_>> {
    let response = match event.method.as_deref()? {
        "network.beforeRequestSent" => false,
        "network.responseStarted" => true,
        _ => return None,
    };
    let params = event.params.as_ref()?;
    if params.get("isBlocked") != Some(&Value::Bool(true)) {
        return None;
    }
    let headers = params
        .pointer("/response/headers")
        .and_then(Value::as_array);
    Some(Blocked {
        request: params.pointer("/request/request")?.as_str()?.to_string(),
        url: params.pointer("/request/url")?.as_str()?.to_string(),
        intercepts: params.get("intercepts")?.as_array()?,
        response_headers: response.then(|| headers.map_or(&[][..], Vec::as_slice)),
        status: params
            .pointer("/response/status")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    })
}

/// The command answering `blocked`, when one of the tab's intercepts
/// blocked it: a request gets a mock's response or goes on; a response
/// gets an upstream route's changes or goes on as it is.
fn answered(blocked: &Blocked<'_>, routes: &mut RouteState) -> Option<(&'static str, Value)> {
    let ours = |intercept: &Option<String>| {
        intercept
            .as_deref()
            .is_some_and(|id| blocked.intercepts.iter().any(|i| i == id))
    };
    let request = &blocked.request;
    let Some(headers) = blocked.response_headers else {
        ours(&routes.intercept).then_some(())?;
        let Some(response) = routes.answer(&blocked.url) else {
            return Some(("network.continueRequest", json!({"request": request})));
        };
        if routes.provided.len() == MAX_PROVIDED {
            routes.provided.pop_front();
        }
        routes.provided.push_back(request.clone());
        return Some(("network.provideResponse", provide(request, &response)));
    };
    ours(&routes.response_intercept).then_some(())?;
    // A mock's response: reported blocked, but not.
    if let Some(at) = routes.provided.iter().position(|id| id == request) {
        routes.provided.remove(at);
        return None;
    }
    let mut params = json!({"request": request});
    let located = headers.iter().any(|h| {
        let name = h.get("name").and_then(Value::as_str).unwrap_or_default();
        name.eq_ignore_ascii_case("location")
    });
    if is_redirect(blocked.status, located) {
        return Some(("network.continueResponse", params));
    }
    if let Some(changes) = routes.changes(&blocked.url) {
        if let Some(status) = changes.status {
            params["statusCode"] = json!(status);
            params["reasonPhrase"] = json!(reason_phrase(status));
        }
        if changes.changes_headers() {
            params["headers"] = Value::Array(merged(headers, &changes));
        }
    }
    Some(("network.continueResponse", params))
}

/// A response's BiDi `headers` with `changes` made: the headers it sets or
/// removes dropped (any value type kept for the others), the ones it sets
/// appended. A string value Firefox joined with line breaks (a repeated
/// `Set-Cookie`) goes back as one header per line: Firefox refuses a line
/// break in a header value it is given.
fn merged(headers: &[Value], changes: &ResponseOverride) -> Vec<Value> {
    let named = |header: &&Value| {
        let name = header
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        changes.names(name)
    };
    let kept = headers.iter().filter(|h| !named(h)).flat_map(one_per_line);
    kept.chain(bidi_headers(&changes.headers)).collect()
}

/// `header`, or one header per line of its string value.
fn one_per_line(header: &Value) -> Vec<Value> {
    let string = header.pointer("/value/type") == Some(&json!("string"));
    let value = header.pointer("/value/value").and_then(Value::as_str);
    match (
        value.filter(|v| string && v.contains('\n')),
        header.get("name"),
    ) {
        (Some(value), Some(name)) => value
            .split('\n')
            .map(|line| json!({"name": name, "value": {"type": "string", "value": line}}))
            .collect(),
        _ => vec![header.clone()],
    }
}

/// BiDi's `[{name, value: {type: "string", value}}]` of `headers`.
fn bidi_headers(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": {"type": "string", "value": value}}))
        .collect()
}

/// `network.provideResponse`'s params for `response`.
fn provide(request: &str, response: &MockResponse) -> Value {
    let headers = bidi_headers(&response.headers);
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

    /// #193 (Firefox): an upstream route adds a responseStarted intercept;
    /// a response it blocks gets the latest matching route's status and
    /// merged headers (a header of another value type kept, a value Firefox
    /// joined with a line break split again), any other one goes on as it
    /// is, but a mock's (reported blocked, though it is not); once no route
    /// is upstream, the response intercept
    /// goes and the request one stays for the mock route.
    #[tokio::test]
    async fn firefox_changes_blocked_responses_of_upstream_routes() {
        // Given a tab with a mock route, then an upstream route
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let next = count.clone();
        mock.bidi().respond_with("network.addIntercept", move |_| {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json!({"intercept": format!("i{n}")})
        });
        let handler = ts::handler(&mock);
        let routes = [
            json!({"url_pattern": "*/mock/*", "body": "m"}),
            json!({"url_pattern": "*/api/*", "upstream": true, "status": 503,
                "headers": [{"name": "Cache-Control", "value": "no-store"}], "remove_headers": ["etag"]}),
        ];
        for args in routes {
            mock_route(ts::call("mock_route", args), &handler)
                .await
                .unwrap();
        }
        let phases: Vec<Value> = mock
            .bidi()
            .commands_named("network.addIntercept")
            .iter()
            .map(|c| c.params["phases"].clone())
            .collect();
        assert_eq!(
            phases,
            [json!(["beforeRequestSent"]), json!(["responseStarted"])]
        );
        // When Firefox blocks a response the upstream route matches, and another one
        let blocked = |id: &str, url: &str| {
            json!({"context": "ctx-0", "isBlocked": true, "intercepts": ["i2"], "navigation": null,
                "redirectCount": 0, "timestamp": 0,
                "request": {"request": id, "url": url, "method": "GET", "headers": [], "cookies": [],
                    "headersSize": 0, "bodySize": 0, "timings": {}},
                "response": {"url": url, "status": 200, "headers": [
                    {"name": "ETag", "value": {"type": "string", "value": "v1"}},
                    {"name": "X-Bin", "value": {"type": "base64", "value": "AAE="}},
                    {"name": "Set-Cookie", "value": {"type": "string", "value": "a=1\nb=2"}},
                    {"name": "cache-control", "value": {"type": "string", "value": "max-age=9"}}]}})
        };
        // (and the response of a request the mock answered, which is not)
        let request = |id: &str, url: &str| {
            let mut event = blocked(id, url);
            event["intercepts"] = json!(["i1"]);
            event.as_object_mut().unwrap().remove("response");
            event
        };
        let events = [
            (
                "network.beforeRequestSent",
                request("R0", "http://a/mock/1"),
            ),
            ("network.responseStarted", blocked("R0", "http://a/mock/1")),
            ("network.responseStarted", blocked("R1", "http://a/api/x")),
            ("network.responseStarted", blocked("R2", "http://a/page")),
        ];
        for (method, event) in events {
            mock.bidi().emit(method, event);
        }
        for _ in 0..100 {
            if mock.bidi().count("network.continueResponse") > 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Then the first gets the route's changes, the second goes on as it is
        let continued = mock.bidi().commands_named("network.continueResponse");
        assert_eq!(continued.len(), 2);
        let provided = mock.bidi().commands_named("network.provideResponse");
        assert_eq!(provided[0].params["request"], "R0");
        assert_eq!(
            continued[0].params,
            json!({"request": "R1", "statusCode": 503, "reasonPhrase": "Service Unavailable", "headers": [
                {"name": "X-Bin", "value": {"type": "base64", "value": "AAE="}},
                {"name": "Set-Cookie", "value": {"type": "string", "value": "a=1"}},
                {"name": "Set-Cookie", "value": {"type": "string", "value": "b=2"}},
                {"name": "Cache-Control", "value": {"type": "string", "value": "no-store"}}]})
        );
        assert_eq!(continued[1].params, json!({"request": "R2"}));
        // When the upstream route goes, its intercept goes, the other stays
        remove_route(
            ts::call("remove_route", json!({"route_id": "route-2"})),
            &handler,
        )
        .await
        .unwrap();
        let removed = mock.bidi().commands_named("network.removeIntercept");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].params, json!({"intercept": "i2"}));
        ts::stop(&handler).await;
    }

    /// #193 (Firefox): a redirect an upstream route matches goes on as it
    /// is (no hit): the changes go to the response at its end, as on
    /// Chrome.
    #[test]
    fn firefox_leaves_redirects_alone() {
        let mut routes = RouteState::default();
        let changes = ResponseOverride {
            status: Some(418),
            ..Default::default()
        };
        routes.add("*", RouteAction::Upstream(changes)).unwrap();
        routes.response_intercept = Some("i2".into());
        let event = |status: u16| -> WsResponse {
            serde_json::from_value(json!({"method": "network.responseStarted", "params": {
                "isBlocked": true, "intercepts": ["i2"], "request": {"request": "R1", "url": "http://a/r"},
                "response": {"status": status, "headers": [
                    {"name": "Location", "value": {"type": "string", "value": "/next"}}]}}}))
            .unwrap()
        };
        let answer = |status: u16, routes: &mut RouteState| {
            let event = event(status);
            answered(&blocked(&event).unwrap(), routes)
        };
        assert_eq!(
            answer(302, &mut routes),
            Some(("network.continueResponse", json!({"request": "R1"})))
        );
        assert_eq!(routes.to_json()["routes"][0]["hits"], 0);
        let (_, params) = answer(200, &mut routes).unwrap();
        assert_eq!(params["statusCode"], 418);
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

    /// NET-08 (#108, Firefox): Firefox's offline emulation fails mocked
    /// requests too, so set_offline with routes, and mock_route while
    /// offline, say so; going online, or offline without routes, does not.
    #[tokio::test]
    async fn firefox_says_mocked_routes_fail_while_offline() {
        // Given a Firefox tab
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        mock.bidi()
            .respond_with("network.addIntercept", |_| json!({"intercept": "i1"}));
        let handler = ts::handler(&mock);
        let offline =
            |on: bool| set_offline(ts::call("set_offline", json!({"offline": on})), &handler);
        let route = |glob: &str| {
            mock_route(
                ts::call("mock_route", json!({"url_pattern": glob})),
                &handler,
            )
        };
        let noted = |reply: &CallToolResult| {
            // The reply object still follows the first lines
            assert!(ts::json_of(reply).is_object(), "{}", ts::text(reply));
            ts::text(reply).lines().nth(1) == Some(OFFLINE_ROUTES)
        };
        // When it goes offline without routes, Then no note
        assert!(!noted(&offline(true).await.unwrap()));
        // When a route is added while offline, Then the note
        assert!(noted(&route("*/a/*").await.unwrap()));
        // When it goes online, Then no note; a route added then, none either
        assert!(!noted(&offline(false).await.unwrap()));
        assert!(!noted(&route("*/b/*").await.unwrap()));
        // When it goes offline with routes, Then the note
        assert!(noted(&offline(true).await.unwrap()));
        ts::stop(&handler).await;
    }
}
