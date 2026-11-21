//! `mock_route`, `list_routes` and `remove_route`: the shared arguments and
//! replies, and the Chrome implementation (`Fetch` domain: a paused request
//! is fulfilled from the tab's routes or continued; a paused response, of
//! an upstream route, gets its changes or is continued).

use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::error_kind;
use crate::handler::routes::{
    Headers, MAX_BODY, MockResponse, ResponseOverride, RouteAction, RouteState, is_redirect,
};
use base64::Engine;
use cdp_browser_lite::NoParams;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

#[macros::mcp_tool(
    name = "mock_route",
    description = "Answers the tab's requests whose URL matches the 'url_pattern' glob (* any characters, ? one, \\ escapes) with a mocked response instead of the network; with upstream: true, lets them reach the server and changes its response instead (status, headers set or removed; its body kept). The latest matching route wins, until remove_route or the tab closes. Returns the route id. On Chrome it cannot run while enable_proxy_auth is active.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct MockRouteTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// URL glob, e.g. '*/api/users*'.
    pub url_pattern: String,
    /// HTTP status, 100 to 599. Default: 200 (upstream: the server's).
    pub status: Option<u16>,
    /// Response body as text, at most 1 MB. Default: empty. Not with upstream.
    pub body: Option<String>,
    /// Content-Type header, e.g. 'application/json'.
    pub content_type: Option<String>,
    /// Other response headers, in order (upstream: each replaces the server's of its name).
    pub headers: Option<Vec<Header>>,
    /// Let the requests reach the server and change its response: status,
    /// content_type, headers and remove_headers apply to it. Default: false.
    pub upstream: Option<bool>,
    /// With upstream: names of the server's response headers to remove.
    pub remove_headers: Option<Vec<String>>,
}

/// One response header of `mock_route`.
#[derive(Debug, Clone, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct Header {
    /// Header name, e.g. 'Cache-Control'.
    pub name: String,
    /// Header value.
    pub value: String,
}

#[macros::mcp_tool(
    name = "list_routes",
    description = "Lists the tab's mocked routes: {routes: [{route_id, url_pattern, status, hits, upstream?}]}, oldest first; hits counts the requests each answered (an upstream route: the responses it changed).",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ListRoutesTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
}

#[macros::mcp_tool(
    name = "remove_route",
    description = "Removes the tab's mocked route 'route_id', or every route when omitted: matching requests reach the network again.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct RemoveRouteTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// The id mock_route returned; omit to remove every route.
    pub route_id: Option<String>,
}

impl MockRouteTool {
    /// The checked route (before any browser work).
    pub(crate) fn action(&self) -> Result<RouteAction, CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        if self.url_pattern.is_empty() {
            return fail("'url_pattern' is empty: use '*' to match every request".into());
        }
        if let Some(status) = self.status.filter(|s| !(100..=599).contains(s)) {
            return fail(format!("'status' must be 100 to 599, got {status}"));
        }
        let headers = self
            .checked_headers()
            .map_err(CallToolError::from_message)?;
        if self.upstream.unwrap_or(false) {
            return self.upstream_action(headers);
        }
        if self.remove_headers.is_some() {
            return fail("'remove_headers' needs upstream: true (a mocked response has only the headers given)".into());
        }
        let body = self.body.clone().unwrap_or_default();
        if body.len() > MAX_BODY {
            return fail(format!(
                "'body' has {} bytes; at most {MAX_BODY}",
                body.len()
            ));
        }
        Ok(RouteAction::Mock(MockResponse {
            status: self.status.unwrap_or(200),
            headers,
            body,
        }))
    }

    /// The changes of an upstream route: at least one, and no body (the
    /// server's is kept).
    fn upstream_action(&self, headers: Headers) -> Result<RouteAction, CallToolError> {
        let fail = |message: &str| Err(CallToolError::from_message(message));
        if self.body.is_some() {
            return fail(
                "'body' cannot be used with upstream: true (the server's body is kept); leave upstream out to mock the whole response",
            );
        }
        let remove = self.remove_headers.clone().unwrap_or_default();
        if remove
            .iter()
            .any(|name| name.is_empty() || name.contains(['\r', '\n']))
        {
            return fail("a name in 'remove_headers' is not valid (empty, or a line break)");
        }
        let changes = ResponseOverride {
            status: self.status,
            headers,
            remove,
        };
        if changes.status.is_none() && !changes.changes_headers() {
            return fail(
                "upstream: true changes nothing: give 'status', 'content_type', 'headers' or 'remove_headers'",
            );
        }
        Ok(RouteAction::Upstream(changes))
    }

    /// `content_type` then `headers` (whose own Content-Type it replaces),
    /// each checked: a name, and no line break.
    fn checked_headers(&self) -> Result<Headers, String> {
        let content_type = self.content_type.iter().map(|ct| ("Content-Type", ct));
        let others = self.headers.iter().flatten().filter(|header| {
            self.content_type.is_none() || !header.name.eq_ignore_ascii_case("content-type")
        });
        let mut headers = Vec::new();
        for (name, value) in content_type.chain(others.map(|h| (h.name.as_str(), &h.value))) {
            let bad = |s: &str| s.contains(['\r', '\n']);
            if name.is_empty() || bad(name) || bad(value) {
                return Err(format!(
                    "header '{name}' is not valid (empty name, or a line break)"
                ));
            }
            headers.push((name.to_string(), value.clone()));
        }
        Ok(headers)
    }

    /// The reply once route `id` does `action`.
    pub(crate) fn reply(&self, id: &str, action: &RouteAction) -> CallToolResult {
        let pattern = &self.url_pattern;
        let (first, body) = match action {
            RouteAction::Mock(response) => (
                format!(
                    "Route {id} added: requests matching '{pattern}' get {} with {} bytes, without reaching the network.",
                    response.status,
                    response.body.len()
                ),
                json!({"route_id": id, "url_pattern": pattern, "status": response.status}),
            ),
            RouteAction::Upstream(changes) => (
                format!(
                    "Route {id} added: requests matching '{pattern}' reach the server, and its response gets {}.",
                    described(changes)
                ),
                json!({"route_id": id, "url_pattern": pattern, "status": changes.status, "upstream": true}),
            ),
        };
        text(&first, &body)
    }
}

/// What an upstream route changes, in words.
fn described(changes: &ResponseOverride) -> String {
    let status = changes.status.map(|s| format!("status {s}"));
    let set =
        (!changes.headers.is_empty()).then(|| format!("{} header(s) set", changes.headers.len()));
    let gone = (!changes.remove.is_empty()).then(|| format!("{} removed", changes.remove.len()));
    let parts: Vec<String> = [status, set, gone].into_iter().flatten().collect();
    parts.join(", ")
}

/// `list_routes`' reply from the tab's routes.
pub(crate) fn listed(routes: &RouteState) -> CallToolResult {
    let listed = routes.to_json();
    let count = listed["routes"].as_array().map_or(0, Vec::len);
    text(&format!("{count} mocked route(s) on the tab."), &listed)
}

/// `remove_route`'s reply.
pub(crate) fn removed(ids: &[String], remaining: &RouteState) -> CallToolResult {
    let left = remaining.patterns().len();
    let first = format!("Removed {} route(s); {left} left on the tab.", ids.len());
    text(&first, &json!({"removed": ids, "remaining": left}))
}

fn text(first: &str, body: &Value) -> CallToolResult {
    let body = serde_json::to_string_pretty(body).unwrap_or_default();
    CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
}

/// Why mocking and proxy authentication do not mix on Chrome.
const PROXY_AUTH_ACTIVE: &str = "enable_proxy_auth is active on this instance, and both need Chrome's Fetch domain: mock routes once its handler stopped (30 s without challenges)";

impl MockRouteTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: MockRouteTool = crate::handler::parse_args(params)?;
        let action = args.action()?;
        let session = handler.session(args.instance_id.clone()).await?;
        if session.proxy_auth_running().await {
            return Err(CallToolError::from_message(PROXY_AUTH_ACTIVE));
        }
        let target = session.target(args.tab_id.clone()).await?;
        let state = session.route_state(args.tab_id.clone())?;
        let added = add_route(&target, &state, &args.url_pattern, action.clone()).await;
        let id = added.map_err(CallToolError::from_message)?;
        Ok(args.reply(&id, &action))
    }
}

impl ListRoutesTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: ListRoutesTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let state = session.route_state(args.tab_id.clone())?;
        Ok(listed(&*state.lock().await))
    }
}

impl RemoveRouteTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: RemoveRouteTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        let state = session.route_state(args.tab_id.clone())?;
        let mut routes = state.lock().await;
        let ids = routes
            .remove(args.route_id.as_deref())
            .map_err(CallToolError::from_message)?;
        let updated = intercept(&target, &mut routes).await;
        updated.map_err(CallToolError::from_message)?;
        Ok(removed(&ids, &routes))
    }
}

/// Adds a route to the tab's table and has Chrome pause the requests of
/// every route; the first route starts the task answering them. Errors
/// are text: the tool error type is not `Send`.
async fn add_route(
    target: &CdpTarget,
    state: &Arc<Mutex<RouteState>>,
    url_pattern: &str,
    action: RouteAction,
) -> Result<String, String> {
    let mut routes = state.lock().await;
    let id = routes.add(url_pattern, action)?;
    if routes.listener.is_empty() {
        // Subscribed before Fetch.enable: a request paused in between
        // would otherwise wait for nobody.
        let events = target.on_domain("Fetch");
        let answering = answer_requests(target.clone(), events, Arc::downgrade(state));
        routes.listener.push(tokio::spawn(answering));
    }
    if let Err(e) = intercept(target, &mut routes).await {
        let _ = routes.remove(Some(&id));
        intercept(target, &mut routes).await?;
        return Err(e);
    }
    Ok(id)
}

/// Has Chrome pause the requests the tab's routes match (the responses,
/// for an upstream route), or stops pausing (and answering) once no route
/// is left.
async fn intercept(target: &CdpTarget, routes: &mut RouteState) -> Result<(), String> {
    if routes.is_empty() {
        let _ = target.send_raw_command("Fetch.disable", NoParams).await;
        routes.listener = Default::default();
        return Ok(());
    }
    let stage = |upstream: bool| if upstream { "Response" } else { "Request" };
    let patterns: Vec<Value> = routes
        .patterns()
        .iter()
        .map(|(p, upstream)| json!({"urlPattern": p, "requestStage": stage(*upstream)}))
        .collect();
    target
        .send_raw_command("Fetch.enable", json!({ "patterns": patterns }))
        .await
        .map(|_| ())
        .map_err(|e| format!("Chrome refused Fetch.enable: {}", error_kind(&e)))
}

/// Answers the requests Chrome pauses: fulfilled from the latest matching
/// route, or continued; a paused response (of an upstream route) gets the
/// latest matching route's changes, or is continued as it is. Holds the
/// routes weakly: they own this task.
async fn answer_requests(
    target: CdpTarget,
    mut events: cdp_browser_lite::EventFilter,
    routes: Weak<Mutex<RouteState>>,
) {
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(cdp_browser_lite::CdpError::Lagged { skipped }) => {
                tracing::warn!(skipped, "mocked routes fell behind; paused requests lost");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = error_kind(&e), "mocked routes: Fetch events ended");
                break;
            }
        };
        let Some(paused) = paused_request(&event) else {
            continue;
        };
        let Some(routes) = routes.upgrade() else {
            // The routes went (the connection is being reset): let it
            // through, as with no route, rather than leave it paused.
            let (method, params) = answered(&paused, &mut RouteState::default());
            let _ = target.send_raw_command(method, params).await;
            break;
        };
        let (method, params) = answered(&paused, &mut *routes.lock().await);
        if let Err(e) = target.send_raw_command(method, params).await {
            tracing::warn!(method, error = error_kind(&e), "mocked route answer failed");
        }
    }
}

/// A request Chrome paused (`Fetch.requestPaused`).
#[derive(Debug, PartialEq)]
struct Paused {
    request: String,
    url: String,
    /// At the response stage, the server's response; `None` at the
    /// request stage.
    response: Option<PausedResponse>,
}

/// A response Chrome paused: its status and headers (each value of a
/// repeated header on a line of its own, as Chrome joins them), or the
/// network error that left it without one.
#[derive(Debug, PartialEq)]
struct PausedResponse {
    status: Option<u64>,
    headers: Headers,
    error: Option<String>,
}

/// The request id and URL of a `Fetch.requestPaused` event, and its
/// response when it is paused at the response stage (it has a status, or
/// a network error that left it without one).
fn paused_request(event: &cdp_browser_lite::WsResponse) -> Option<Paused> {
    if event.method.as_deref() != Some("Fetch.requestPaused") {
        return None;
    }
    let params = event.params.as_ref()?;
    let status = params.get("responseStatusCode").and_then(Value::as_u64);
    let error = params.get("responseErrorReason").and_then(Value::as_str);
    let headers = params.get("responseHeaders").and_then(Value::as_array);
    let pairs = headers.into_iter().flatten().filter_map(|header| {
        let name = header.get("name")?.as_str()?;
        Some((name.to_string(), header.get("value")?.as_str()?.to_string()))
    });
    let response = (status.is_some() || error.is_some()).then(|| PausedResponse {
        status,
        headers: pairs.collect(),
        error: error.map(str::to_string),
    });
    Some(Paused {
        request: params.get("requestId")?.as_str()?.to_string(),
        url: params.pointer("/request/url")?.as_str()?.to_string(),
        response,
    })
}

/// The command answering `paused` from `routes`: at the request stage a
/// mock's response, or let through; at the response stage an upstream
/// route's changes, or let through as it is (a network error as it was).
fn answered(paused: &Paused, routes: &mut RouteState) -> (&'static str, Value) {
    let request = &paused.request;
    let Some(response) = &paused.response else {
        return match routes.answer(&paused.url) {
            Some(response) => ("Fetch.fulfillRequest", fulfill(request, &response)),
            None => ("Fetch.continueRequest", json!({"requestId": request})),
        };
    };
    if let Some(error) = &response.error {
        return (
            "Fetch.failRequest",
            json!({"requestId": request, "errorReason": error}),
        );
    }
    let mut params = json!({"requestId": request});
    let located = response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("location"));
    if is_redirect(response.status.unwrap_or_default(), located) {
        return ("Fetch.continueResponse", params);
    }
    if let Some(changes) = routes.changes(&paused.url) {
        // Chrome takes both or neither ("Cannot override only status or
        // headers").
        params["responseCode"] = json!(
            changes
                .status
                .map_or(response.status, |s| Some(u64::from(s)))
        );
        params["responseHeaders"] = header_list(&changes.merged(split_lines(&response.headers)));
    }
    ("Fetch.continueResponse", params)
}

/// `headers` with each value Chrome joined with line breaks (a repeated
/// header such as `Set-Cookie`) as one header per line again: Chrome
/// refuses a line break in a header value it is given.
fn split_lines(headers: &[(String, String)]) -> Vec<(String, String)> {
    let lines = |(name, value): &(String, String)| {
        let values = value
            .split('\n')
            .map(|line| (name.clone(), line.to_string()));
        values.collect::<Vec<_>>()
    };
    headers.iter().flat_map(lines).collect()
}

/// CDP's `[{name, value}]` of `headers`.
fn header_list(headers: &[(String, String)]) -> Value {
    let list: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    Value::Array(list)
}

/// `Fetch.fulfillRequest`'s params for `response` (the body in base64).
fn fulfill(request: &str, response: &MockResponse) -> Value {
    let headers = header_list(&response.headers);
    let body = base64::engine::general_purpose::STANDARD.encode(&response.body);
    json!({"requestId": request, "responseCode": response.status,
           "responseHeaders": headers, "body": body})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };

    /// NET-04: the arguments are checked before any browser work, and
    /// content_type is the one Content-Type header.
    #[test]
    fn arguments_are_checked() {
        let tool = |args: Value| -> MockRouteTool { serde_json::from_value(args).unwrap() };
        let fails = |args: Value| tool(args).action().unwrap_err().to_string();
        assert!(fails(json!({"url_pattern": ""})).contains("'*'"));
        assert!(fails(json!({"url_pattern": "*", "status": 99})).contains("100 to 599"));
        let huge = "x".repeat(MAX_BODY + 1);
        assert!(fails(json!({"url_pattern": "*", "body": huge})).contains("at most"));
        let broken =
            json!({"url_pattern": "*", "headers": [{"name": "X-A", "value": "1\r\nX-B: 2"}]});
        assert!(fails(broken).contains("line break"));
        let response = tool(
            json!({"url_pattern": "*", "content_type": "application/json",
            "headers": [{"name": "content-type", "value": "text/html"}, {"name": "X-Mock", "value": "1"}]}),
        )
        .action()
        .unwrap();
        assert_eq!(
            response,
            RouteAction::Mock(MockResponse {
                status: 200,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("X-Mock".into(), "1".into())
                ],
                body: String::new(),
            })
        );
    }

    /// #193: an upstream route keeps the server's body, so a body is
    /// refused; it must change something; remove_headers needs it; its
    /// status and headers are optional changes.
    #[test]
    fn upstream_arguments_are_checked() {
        let tool = |args: Value| -> MockRouteTool { serde_json::from_value(args).unwrap() };
        let fails = |args: Value| tool(args).action().unwrap_err().to_string();
        let upstream = |more: Value| {
            let mut args = json!({"url_pattern": "*/api/*", "upstream": true});
            args.as_object_mut()
                .unwrap()
                .extend(more.as_object().unwrap().clone());
            args
        };
        assert!(fails(upstream(json!({"body": "x"}))).contains("server's body is kept"));
        assert!(fails(upstream(json!({}))).contains("changes nothing"));
        assert!(fails(upstream(json!({"status": 600}))).contains("100 to 599"));
        assert!(fails(upstream(json!({"remove_headers": [""]}))).contains("not valid"));
        assert!(
            fails(json!({"url_pattern": "*", "remove_headers": ["Etag"]}))
                .contains("upstream: true")
        );
        let action = tool(upstream(
            json!({"status": 503, "content_type": "text/plain",
            "remove_headers": ["Cache-Control"]}),
        ))
        .action()
        .unwrap();
        assert_eq!(
            action,
            RouteAction::Upstream(ResponseOverride {
                status: Some(503),
                headers: vec![("Content-Type".into(), "text/plain".into())],
                remove: vec!["Cache-Control".into()],
            })
        );
        let reply = tool(upstream(json!({"status": 503}))).reply("route-1", &action);
        assert!(reply_text(&reply).contains(
            "reach the server, and its response gets status 503, 1 header(s) set, 1 removed"
        ));
    }

    /// #193 (Chrome): an upstream route pauses the responses its pattern
    /// matches, not the requests; a paused response it matches gets its
    /// status and its headers merged (a header Chrome joined with a line
    /// break sent as one header per line again); any other paused response
    /// is continued as it is; a mock route on the tab still answers.
    #[tokio::test]
    async fn chrome_changes_paused_responses_of_upstream_routes() {
        // Given a tab with an upstream route, then a mock route
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let upstream = json!({"url_pattern": "*/api/*", "upstream": true, "status": 503,
            "headers": [{"name": "Cache-Control", "value": "no-store"}], "remove_headers": ["etag"]});
        let mocked = json!({"url_pattern": "*/api/mock*", "body": "m"});
        for args in [upstream, mocked] {
            MockRouteTool::handle(call("mock_route", args), &handler)
                .await
                .unwrap();
        }
        let enabled = &mock.commands_named("Fetch.enable")[1].params;
        assert_eq!(
            enabled["patterns"],
            json!([{"urlPattern": "*/api/*", "requestStage": "Response"},
                   {"urlPattern": "*/api/mock*", "requestStage": "Request"}])
        );
        // When Chrome pauses responses, and a mocked request
        for (id, url) in [("R1", "http://a/api/x"), ("R2", "http://a/other")] {
            mock.emit("Fetch.requestPaused", paused_response(id, url), None);
        }
        let request = json!({"requestId": "R3", "request": {"url": "http://a/api/mock1", "method": "GET"}, "resourceType": "Fetch"});
        mock.emit("Fetch.requestPaused", request, None);
        until_sent(mock, "Fetch.fulfillRequest").await;
        // Then the route's response gets its changes, the other goes as it is
        let continued = mock.commands_named("Fetch.continueResponse");
        assert_eq!(
            continued[0].params,
            json!({"requestId": "R1", "responseCode": 503, "responseHeaders": [
                {"name": "Set-Cookie", "value": "a=1"}, {"name": "Set-Cookie", "value": "b=2"},
                {"name": "Cache-Control", "value": "no-store"}]})
        );
        assert_eq!(continued[1].params, json!({"requestId": "R2"}));
        // And the mock still answers its requests
        let fulfilled = &mock.commands_named("Fetch.fulfillRequest")[0].params;
        assert_eq!(fulfilled["requestId"], "R3");
        let reply = ListRoutesTool::handle(call("list_routes", json!({})), &handler)
            .await
            .unwrap();
        let listed: Value =
            serde_json::from_str(reply_text(&reply).split_once('\n').unwrap().1).unwrap();
        assert_eq!(listed["routes"][0]["hits"], 1);
        assert_eq!(listed["routes"][0]["upstream"], true);
    }

    /// NET-05 (Chrome): a route makes Chrome pause the requests it
    /// matches; a paused request is fulfilled from the route (body in
    /// base64), any other one continued; removing the last route disables
    /// Fetch.
    #[tokio::test]
    async fn chrome_fulfills_paused_requests_from_routes() {
        // Given a tab with a route for */api/*
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let args = json!({"url_pattern": "*/api/*", "status": 201, "body": "{\"ok\":1}",
            "content_type": "application/json"});
        let reply = MockRouteTool::handle(call("mock_route", args), &handler)
            .await
            .unwrap();
        assert!(reply_text(&reply).contains("Route route-1 added"));
        let enabled = &mock.commands_named("Fetch.enable")[0].params;
        assert_eq!(
            enabled["patterns"],
            json!([{"urlPattern": "*/api/*", "requestStage": "Request"}])
        );
        // When Chrome pauses a matching request, and another one
        let paused = |id: &str, url: &str| json!({"requestId": id, "request": {"url": url, "method": "GET"}, "resourceType": "Fetch"});
        mock.emit("Fetch.requestPaused", paused("R1", "http://a/api/x"), None);
        mock.emit("Fetch.requestPaused", paused("R2", "http://a/page"), None);
        for _ in 0..100 {
            if !mock.commands_named("Fetch.continueRequest").is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Then the first is fulfilled from the route, the second continued
        let fulfilled = &mock.commands_named("Fetch.fulfillRequest")[0].params;
        assert_eq!(
            *fulfilled,
            json!({"requestId": "R1", "responseCode": 201, "body": "eyJvayI6MX0=",
                "responseHeaders": [{"name": "Content-Type", "value": "application/json"}]})
        );
        let continued = &mock.commands_named("Fetch.continueRequest")[0].params;
        assert_eq!(*continued, json!({"requestId": "R2"}));
        // And the route counted its hit
        let reply = ListRoutesTool::handle(call("list_routes", json!({})), &handler)
            .await
            .unwrap();
        assert!(
            reply_text(&reply).contains("\"hits\": 1"),
            "{}",
            reply_text(&reply)
        );
        // When the last route is removed, Fetch is disabled
        let reply = RemoveRouteTool::handle(call("remove_route", json!({})), &handler)
            .await
            .unwrap();
        assert!(reply_text(&reply).contains("Removed 1 route(s); 0 left"));
        assert_eq!(mock.commands_named("Fetch.disable").len(), 1);
    }

    /// #193 (Chrome): Chrome takes a response's status and headers together
    /// or neither, so a route changing only one sends the other as the
    /// server gave it.
    #[test]
    fn chrome_gets_status_and_headers_together() {
        let mut routes = teapot_routes();
        let server = json!([{"name": "ETag", "value": "\"v1\""}, {"name": "Set-Cookie", "value": "a=1"},
            {"name": "Set-Cookie", "value": "b=2"}, {"name": "cache-control", "value": "max-age=9"}]);
        let (method, params) = answer(paused_response("R1", "http://a/teapot"), &mut routes);
        assert_eq!(method, "Fetch.continueResponse");
        assert_eq!(
            params,
            json!({"requestId": "R1", "responseCode": 418, "responseHeaders": server})
        );
        let (_, params) = answer(paused_response("R2", "http://a/plain"), &mut routes);
        assert_eq!(params["responseCode"], 200);
        assert_eq!(params["responseHeaders"].as_array().unwrap().len(), 3);
    }

    /// #193 (Chrome): a redirect is left alone (the changes go to the
    /// response at its end, as on Firefox), and a response a network error
    /// left without a status fails with that error.
    #[test]
    fn chrome_leaves_redirects_and_network_errors_alone() {
        let mut routes = teapot_routes();
        let mut redirect = paused_response("R4", "http://a/teapot");
        redirect["responseStatusCode"] = json!(302);
        redirect["responseHeaders"] = json!([{"name": "Location", "value": "/next"}]);
        assert_eq!(
            answer(redirect, &mut routes),
            ("Fetch.continueResponse", json!({"requestId": "R4"}))
        );
        let failed = json!({"requestId": "R3", "request": {"url": "http://a/teapot", "method": "GET"},
            "resourceType": "Fetch", "responseErrorReason": "ConnectionRefused"});
        assert_eq!(
            answer(failed, &mut routes),
            (
                "Fetch.failRequest",
                json!({"requestId": "R3", "errorReason": "ConnectionRefused"})
            )
        );
    }

    /// A request Chrome paused after the tab's routes went (its connection
    /// is being reset) is let through, not left paused: the task stops
    /// once it answered it.
    #[tokio::test]
    async fn chrome_lets_paused_requests_through_once_the_routes_are_gone() {
        // Given the answering task of routes that are gone
        let mock = spawn_recording_mock().await;
        let address = format!("127.0.0.1:{}", mock.http_port);
        let timeout = std::time::Duration::from_secs(2);
        let browser = cdp_browser_lite::BrowserClient::connect(&address, timeout)
            .await
            .unwrap();
        let tab = browser.attach("T-page-1").await.unwrap();
        let target = CdpTarget::tab(tab.clone());
        let gone = Arc::downgrade(&Arc::new(Mutex::new(RouteState::default())));
        let task = tokio::spawn(answer_requests(
            target.clone(),
            target.on_domain("Fetch"),
            gone,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // When Chrome pauses a response
        let session = Some(tab.session_id());
        mock.emit(
            "Fetch.requestPaused",
            paused_response("R1", "http://a/x"),
            session,
        );
        until_sent(mock, "Fetch.continueResponse").await;
        // Then it goes on as it is, and the task ends
        let continued = mock.commands_named("Fetch.continueResponse");
        assert_eq!(continued[0].params, json!({"requestId": "R1"}));
        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), task).await;
        assert!(ended.is_ok());
    }

    /// Upstream routes: `*/teapot*` sets status 418, `*/plain*` removes
    /// the ETag.
    fn teapot_routes() -> RouteState {
        let mut routes = RouteState::default();
        let status_only = ResponseOverride {
            status: Some(418),
            ..Default::default()
        };
        let headers_only = ResponseOverride {
            remove: vec!["etag".into()],
            ..Default::default()
        };
        routes
            .add("*/teapot*", RouteAction::Upstream(status_only))
            .unwrap();
        routes
            .add("*/plain*", RouteAction::Upstream(headers_only))
            .unwrap();
        routes
    }

    /// What answers the `Fetch.requestPaused` of `params` from `routes`.
    fn answer(params: Value, routes: &mut RouteState) -> (&'static str, Value) {
        let event = json!({"method": "Fetch.requestPaused", "params": params});
        let event: cdp_browser_lite::WsResponse = serde_json::from_value(event).unwrap();
        answered(&paused_request(&event).unwrap(), routes)
    }

    /// A `Fetch.requestPaused` of request `id` to `url` at the response
    /// stage: a 200 with an ETag, two cookies (Chrome joins them with a
    /// line break) and a Cache-Control.
    fn paused_response(id: &str, url: &str) -> Value {
        json!({"requestId": id, "request": {"url": url, "method": "GET"},
            "resourceType": "Fetch", "responseStatusCode": 200, "responseHeaders": [
                {"name": "ETag", "value": "\"v1\""}, {"name": "Set-Cookie", "value": "a=1\nb=2"},
                {"name": "cache-control", "value": "max-age=9"}]})
    }

    /// Waits (at most 1 s) until the mock received a `method` command.
    async fn until_sent(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        method: &str,
    ) {
        for _ in 0..100 {
            if !mock.commands_named(method).is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// NET-06 (Chrome): routes refuse to start while proxy authentication
    /// owns the Fetch domain.
    #[tokio::test]
    async fn chrome_routes_refuse_while_proxy_auth_runs() {
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let mut tasks = crate::handler::cdp_domains::event_pump::ListenerHandles::default();
        tasks.push(tokio::spawn(std::future::pending::<()>()));
        session.proxy_auth_tasks.lock().await.insert(None, tasks);
        let args = json!({"url_pattern": "*"});
        let error = MockRouteTool::handle(call("mock_route", args), &handler)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("enable_proxy_auth"), "{error}");
        assert!(mock.commands_named("Fetch.enable").is_empty());
    }
}
