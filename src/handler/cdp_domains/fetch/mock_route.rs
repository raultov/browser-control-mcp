//! `mock_route`, `list_routes` and `remove_route`: the shared arguments and
//! replies, and the Chrome implementation (`Fetch` domain: a paused request
//! is fulfilled from the tab's routes or continued; failed when Chrome
//! refuses the route's response).

use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::error_kind;
use crate::handler::routes::{MAX_BODY, MockResponse, RouteState, reason_phrase};
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
    description = "Answers the tab's requests whose URL matches the 'url_pattern' glob (* any characters, ? one, \\ escapes) with a mocked response instead of the network; the latest matching route wins, until remove_route or the tab closes. A response the browser refuses fails the request (list_routes shows the browser's error). Returns the route id. On Chrome it cannot run while enable_proxy_auth is active.",
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
    /// HTTP status, 100 to 599. Default: 200.
    pub status: Option<u16>,
    /// Response body as text, at most 1 MB. Default: empty.
    pub body: Option<String>,
    /// Content-Type header, e.g. 'application/json'.
    pub content_type: Option<String>,
    /// Other response headers, in order.
    pub headers: Option<Vec<Header>>,
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
    description = "Lists the tab's mocked routes: {routes: [{route_id, url_pattern, status, hits, errors?, last_error?}]}, oldest first; hits counts the requests each answered; errors and last_error appear once the browser refused a route's response (the request then failed).",
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
    /// The checked response (before any browser work).
    pub(crate) fn response(&self) -> Result<MockResponse, CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        if self.url_pattern.is_empty() {
            return fail("'url_pattern' is empty: use '*' to match every request".into());
        }
        let status = self.status.unwrap_or(200);
        if !(100..=599).contains(&status) {
            return fail(format!("'status' must be 100 to 599, got {status}"));
        }
        let body = self.body.clone().unwrap_or_default();
        if body.len() > MAX_BODY {
            return fail(format!(
                "'body' has {} bytes; at most {MAX_BODY}",
                body.len()
            ));
        }
        let content_type = self.content_type.iter().map(|ct| ("Content-Type", ct));
        let others = self.headers.iter().flatten().filter(|header| {
            self.content_type.is_none() || !header.name.eq_ignore_ascii_case("content-type")
        });
        let mut headers = Vec::new();
        for (name, value) in content_type.chain(others.map(|h| (h.name.as_str(), &h.value))) {
            let bad = |s: &str| s.contains(['\r', '\n']);
            if name.is_empty() || bad(name) || bad(value) {
                return fail(format!(
                    "header '{name}' is not valid (empty name, or a line break)"
                ));
            }
            headers.push((name.to_string(), value.clone()));
        }
        Ok(MockResponse {
            status,
            headers,
            body,
        })
    }

    /// The reply once route `id` answers with `response`.
    pub(crate) fn reply(&self, id: &str, response: &MockResponse) -> CallToolResult {
        let first = format!(
            "Route {id} added: requests matching '{}' get {} with {} bytes, without reaching the network.",
            self.url_pattern,
            response.status,
            response.body.len()
        );
        let body =
            json!({"route_id": id, "url_pattern": self.url_pattern, "status": response.status});
        text(&first, &body)
    }
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
        let response = args.response()?;
        let session = handler.session(args.instance_id.clone()).await?;
        if session.proxy_auth_running().await {
            return Err(CallToolError::from_message(PROXY_AUTH_ACTIVE));
        }
        let target = session.target(args.tab_id.clone()).await?;
        let state = session.route_state(args.tab_id.clone())?;
        let added = add_route(&target, &state, &args.url_pattern, response.clone()).await;
        let id = added.map_err(CallToolError::from_message)?;
        Ok(args.reply(&id, &response))
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
    response: MockResponse,
) -> Result<String, String> {
    let mut routes = state.lock().await;
    let id = routes.add(url_pattern, response)?;
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

/// Has Chrome pause the requests the tab's routes match, or stops
/// pausing (and answering) once no route is left.
async fn intercept(target: &CdpTarget, routes: &mut RouteState) -> Result<(), String> {
    if routes.is_empty() {
        let _ = target.send_raw_command("Fetch.disable", NoParams).await;
        routes.listener = Default::default();
        return Ok(());
    }
    let patterns: Vec<Value> = routes
        .patterns()
        .iter()
        .map(|p| json!({"urlPattern": p, "requestStage": "Request"}))
        .collect();
    target
        .send_raw_command("Fetch.enable", json!({ "patterns": patterns }))
        .await
        .map(|_| ())
        .map_err(|e| format!("Chrome refused Fetch.enable: {}", error_kind(&e)))
}

/// Answers the requests Chrome pauses (see [`answer`]). Holds the routes
/// weakly: they own this task.
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
        let Some((request, url)) = paused_request(&event) else {
            continue;
        };
        let Some(routes) = routes.upgrade() else {
            // The routes went (the connection is being reset): let it
            // through, as with no route, rather than leave it paused.
            let params = json!({"requestId": request});
            let _ = target
                .send_raw_command("Fetch.continueRequest", params)
                .await;
            break;
        };
        answer(&target, &routes, &request, &url).await;
    }
}

/// Answers `request` to `url`, which Chrome paused: fulfilled from the
/// latest matching route, or continued. A response Chrome refuses fails
/// the request instead of leaving it paused (#194), and the route keeps
/// Chrome's error for `list_routes` (never for the log).
async fn answer(target: &CdpTarget, routes: &Mutex<RouteState>, request: &str, url: &str) {
    let answer = routes.lock().await.answer(url);
    let Some((route, response)) = answer else {
        let params = json!({"requestId": request});
        if let Err(e) = target
            .send_raw_command("Fetch.continueRequest", params)
            .await
        {
            tracing::warn!(
                error = error_kind(&e),
                "mocked routes: continuing a request failed"
            );
        }
        return;
    };
    let fulfilled = target.send_raw_command("Fetch.fulfillRequest", fulfill(request, &response));
    let Err(error) = fulfilled.await else {
        return;
    };
    tracing::warn!(
        error = error_kind(&error),
        "Chrome refused a mocked response"
    );
    let said = match &error {
        cdp_browser_lite::CdpError::ProtocolError { message, .. } => message.as_str(),
        other => error_kind(other),
    };
    routes.lock().await.note_error(&route, said);
    let failed = json!({"requestId": request, "errorReason": "Failed"});
    let _ = target.send_raw_command("Fetch.failRequest", failed).await;
}

/// The request id and URL of a `Fetch.requestPaused` event.
fn paused_request(event: &cdp_browser_lite::WsResponse) -> Option<(String, String)> {
    if event.method.as_deref() != Some("Fetch.requestPaused") {
        return None;
    }
    let params = event.params.as_ref()?;
    let request = params.get("requestId")?.as_str()?.to_string();
    let url = params.pointer("/request/url")?.as_str()?.to_string();
    Some((request, url))
}

/// `Fetch.fulfillRequest`'s params for `response` (the body in base64),
/// with the status's reason phrase, which Chrome needs.
fn fulfill(request: &str, response: &MockResponse) -> Value {
    let headers: Vec<Value> = response
        .headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    let body = base64::engine::general_purpose::STANDARD.encode(&response.body);
    json!({"requestId": request, "responseCode": response.status,
           "responsePhrase": reason_phrase(response.status),
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
        let fails = |args: Value| tool(args).response().unwrap_err().to_string();
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
        .response()
        .unwrap();
        assert_eq!(
            response,
            MockResponse {
                status: 200,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("X-Mock".into(), "1".into())
                ],
                body: String::new(),
            }
        );
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
            json!({"requestId": "R1", "responseCode": 201, "responsePhrase": "Created",
                "body": "eyJvayI6MX0=",
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

    /// #194 (Chrome): a mocked response Chrome refuses fails the request
    /// instead of leaving it paused, and list_routes shows Chrome's error
    /// on the route.
    #[tokio::test]
    async fn chrome_fails_a_request_whose_mocked_response_it_refuses() {
        // Given a route whose response Chrome refuses
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Fetch.fulfillRequest",
            |_| json!({"error": {"code": -32602, "message": "Invalid http status code or phrase"}}),
        );
        let handler = mock_handler(mock.http_port);
        let args = json!({"url_pattern": "*/api/*", "status": 299, "body": "x"});
        MockRouteTool::handle(call("mock_route", args), &handler)
            .await
            .unwrap();
        // When Chrome pauses a request it matches
        let paused = json!({"requestId": "R1", "request": {"url": "http://a/api/x", "method": "GET"},
            "resourceType": "Fetch"});
        mock.emit("Fetch.requestPaused", paused, None);
        until_sent(mock, "Fetch.failRequest").await;
        // Then the request fails, and the route says why
        let failed = mock.commands_named("Fetch.failRequest");
        assert_eq!(
            failed[0].params,
            json!({"requestId": "R1", "errorReason": "Failed"})
        );
        let reply = ListRoutesTool::handle(call("list_routes", json!({})), &handler)
            .await
            .unwrap();
        let text = reply_text(&reply);
        let listed: Value = serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
        let route = &listed["routes"][0];
        assert_eq!(route["errors"], 1, "{text}");
        assert_eq!(route["last_error"], "Invalid http status code or phrase");
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
        // When Chrome pauses a request
        let paused = json!({"requestId": "R1", "request": {"url": "http://a/x", "method": "GET"},
            "resourceType": "Fetch"});
        mock.emit("Fetch.requestPaused", paused, Some(tab.session_id()));
        until_sent(mock, "Fetch.continueRequest").await;
        // Then it goes on, and the task ends
        let continued = mock.commands_named("Fetch.continueRequest");
        assert_eq!(continued[0].params, json!({"requestId": "R1"}));
        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), task).await;
        assert!(ended.is_ok());
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
