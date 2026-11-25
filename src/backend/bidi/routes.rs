//! `mock_route`, `list_routes`, `remove_route` and `set_offline` on Firefox:
//! a `beforeRequestSent` intercept on the tab blocks its requests while it
//! has routes; a blocked request gets `network.provideResponse` from the
//! latest matching route (`network.failRequest` when Firefox refuses that
//! response, or when the route aborts, #212), or `network.continueRequest`. The answering task outlives
//! the intercept: a request blocked just before `remove_route` took it away
//! is still let through (#195). Offline is
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
    AbortReason, MAX_RETIRED, MockResponse, RouteAction, RouteState, reason_phrase,
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
    if args.error_reason.is_some_and(|r| r != AbortReason::Failed) {
        return Err(CallToolError::from_message(NO_ERROR_REASON));
    }
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let added = add_route(&tab, &args.url_pattern, action.clone()).await;
    let id = added.map_err(CallToolError::from_message)?;
    let offline = tab.routes.lock().await.offline;
    Ok(noted(args.reply(&id, &action, FAILURE), offline))
}

/// Why `error_reason` is refused on Firefox (#212).
const NO_ERROR_REASON: &str = "Firefox cannot choose why a request fails (BiDi's network.failRequest takes no reason): omit 'error_reason' (or give 'failed'); the page then sees a network error, which get_network_logs lists as failed: NS_ERROR_ABORT.";

/// How a request a route fails ends on Firefox (measured on Firefox 160:
/// `fetch()` rejects with a `NetworkError`).
const FAILURE: &str = "a network error (NS_ERROR_ABORT)";

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

/// Adds a route to the tab's table; the first one adds the intercept. A
/// route starts the task answering the requests it blocks unless one runs
/// (it outlives the intercepts, see [`answer_requests`]; one that ended by
/// itself is replaced, or the intercept would block requests nobody
/// answers). Errors are text: the tool error type is not `Send`.
async fn add_route(
    tab: &FirefoxTab,
    url_pattern: &str,
    action: RouteAction,
) -> Result<String, String> {
    let mut routes = tab.routes.lock().await;
    let id = routes.add(url_pattern, action)?;
    let client = tab.tab.client().clone();
    // Subscribed before the intercept: a request blocked in between would
    // otherwise wait for nobody.
    let events = (!routes.listener.is_running()).then(|| client.subscribe());
    if routes.intercept.is_none() {
        let added = page::command(&tab.tab, "network.addIntercept", intercept_params(tab)).await;
        let intercept = added
            .ok()
            .and_then(|reply| reply.get("intercept")?.as_str().map(str::to_string));
        let Some(intercept) = intercept else {
            let _ = routes.remove(Some(&id));
            return Err("Firefox refused network.addIntercept".into());
        };
        routes.intercept = Some(intercept);
    }
    if let Some(events) = events {
        let network = tab.network.clone();
        let answering = answer_requests(client, events, Arc::downgrade(&tab.routes), network);
        routes.listener = Default::default();
        routes.listener.push(tokio::spawn(answering));
    }
    Ok(id)
}

/// The tab's intercept: every request of the tab (and its iframes) is
/// blocked; the routes' globs are matched here, as BiDi patterns are not
/// globs.
fn intercept_params(tab: &FirefoxTab) -> Value {
    json!({"phases": ["beforeRequestSent"], "contexts": [tab.tab.context_id()]})
}

/// Removes the tab's intercept. Firefox keeps blocked what it blocked, so
/// the task keeps answering: the intercept is retired, and what it blocked
/// is let through (#195: `remove_route` with requests in flight left them
/// pending for ever).
async fn stop_intercepting(tab: &FirefoxTab, routes: &mut RouteState) {
    let Some(intercept) = routes.intercept.take() else {
        return;
    };
    if routes.retired.len() == MAX_RETIRED {
        routes.retired.pop_front();
    }
    routes.retired.push_back(intercept.clone());
    let params = json!({"intercept": intercept});
    let _ = page::command(&tab.tab, "network.removeIntercept", params).await;
}

/// Answers the requests the tab's intercepts block (see [`answer`]), as
/// long as the tab's routes live: what its current intercept blocked from
/// the routes, what a retired one blocked let through. Holds the routes
/// weakly: they own this task. `network` is the tab's request log.
async fn answer_requests(
    client: BidiClient,
    mut events: tokio::sync::broadcast::Receiver<WsResponse>,
    routes: Weak<Mutex<RouteState>>,
    network: Arc<Mutex<crate::handler::NetworkState>>,
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
        let Some(blocked) = blocked_request(&event) else {
            continue;
        };
        let Some(routes) = routes.upgrade() else {
            break;
        };
        let blocker = blocker(&*routes.lock().await, &blocked.intercepts);
        match blocker {
            Some(Blocker::Current) => answer(&client, &routes, &network, &blocked).await,
            Some(Blocker::Retired) => {
                let params = json!({"request": blocked.request});
                if client
                    .send_raw_command("network.continueRequest", params)
                    .await
                    .is_err()
                {
                    tracing::warn!("Firefox mocked routes: continuing a request failed");
                }
            }
            None => {}
        }
    }
}

/// Which of the tab's intercepts blocked a request.
#[derive(Debug, PartialEq)]
enum Blocker {
    /// The current one: the routes answer it.
    Current,
    /// One `remove_route` took away: it is let through.
    Retired,
}

/// Which of the tab's intercepts is among `intercepts` (those that blocked
/// a request); `None` when none is the tab's.
fn blocker(routes: &RouteState, intercepts: &[String]) -> Option<Blocker> {
    let blocked_by = |id: &String| intercepts.contains(id);
    if routes.intercept.as_ref().is_some_and(blocked_by) {
        return Some(Blocker::Current);
    }
    routes
        .retired
        .iter()
        .any(blocked_by)
        .then_some(Blocker::Retired)
}

/// Answers a request an intercept `blocked`: a response from the latest
/// matching route, failed as it asks (#212), or let through. The response
/// given is noted in the tab's request log `network`, as Firefox reports
/// none for it (#198: a mocked redirect's hop had no status); a failed
/// request has none. A response Firefox refuses fails the request instead
/// of leaving it blocked (#194), and the route keeps Firefox's error for
/// `list_routes` (never for the log).
async fn answer(
    client: &BidiClient,
    routes: &Mutex<RouteState>,
    network: &Mutex<crate::handler::NetworkState>,
    blocked: &Blocked,
) {
    let (request, url) = (blocked.request.as_str(), blocked.url.as_str());
    let answer = routes.lock().await.answer(url);
    let (route, response) = match answer {
        None => return settle(client, "network.continueRequest", request).await,
        Some((_, RouteAction::Abort(_))) => {
            return settle(client, "network.failRequest", request).await;
        }
        Some((route, RouteAction::Respond(response))) => (route, response),
    };
    let noted = &response.headers;
    network
        .lock()
        .await
        .note_mocked_response(request, response.status, noted);
    let provided = client.send_raw_command("network.provideResponse", provide(request, &response));
    let Err(error) = provided.await else {
        return;
    };
    tracing::warn!("Firefox refused a mocked response");
    let said = match &error {
        bidi_browser_lite::BidiError::ProtocolError { error, message, .. } => {
            format!("{error}: {message}")
        }
        other => other.to_string(),
    };
    routes.lock().await.note_error(&route, &said);
    let failed = json!({"request": request});
    let _ = client.send_raw_command("network.failRequest", failed).await;
}

/// Lets a blocked `request` go on or fails it (`method`); a refusal is
/// only logged.
async fn settle(client: &BidiClient, method: &str, request: &str) {
    let params = json!({"request": request});
    if client.send_raw_command(method, params).await.is_err() {
        tracing::warn!(method, "Firefox mocked routes: a blocked request");
    }
}

/// A request an intercept blocked (`network.beforeRequestSent`).
struct Blocked {
    request: String,
    url: String,
    /// The ids of the intercepts that blocked it.
    intercepts: Vec<String>,
}

/// What a `network.beforeRequestSent` event that blocked a request says.
fn blocked_request(event: &WsResponse) -> Option<Blocked> {
    if event.method.as_deref() != Some("network.beforeRequestSent") {
        return None;
    }
    let params = event.params.as_ref()?;
    if params.get("isBlocked") != Some(&Value::Bool(true)) {
        return None;
    }
    let intercepts = params.get("intercepts")?.as_array()?;
    Some(Blocked {
        request: params.pointer("/request/request")?.as_str()?.to_string(),
        url: params.pointer("/request/url")?.as_str()?.to_string(),
        intercepts: intercepts
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    })
}

/// `network.provideResponse`'s params for `response`, with the status's
/// reason phrase (as on Chrome, which needs one; Firefox gave none).
fn provide(request: &str, response: &MockResponse) -> Value {
    let headers: Vec<Value> = response
        .headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": {"type": "string", "value": value}}))
        .collect();
    json!({"request": request, "statusCode": response.status,
           "reasonPhrase": reason_phrase(response.status), "headers": headers,
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
        for (id, url) in [("R1", "http://a/api/x"), ("R2", "http://a/page")] {
            let event = blocked(id, url, "i1");
            mock.bidi().emit("network.beforeRequestSent", event);
        }
        until_sent(&mock, "network.continueRequest").await;
        // Then the first gets the route's response, the second goes through
        let provided = &mock.bidi().commands_named("network.provideResponse")[0].params;
        assert_eq!(
            *provided,
            json!({"request": "R1", "statusCode": 201, "reasonPhrase": "Created",
                "headers": [{"name": "Content-Type", "value": {"type": "string", "value": "application/json"}}],
                "body": {"type": "string", "value": "{\"ok\":1}"}})
        );
        let continued = &mock.bidi().commands_named("network.continueRequest")[0].params;
        assert_eq!(*continued, json!({"request": "R2"}));
        // #212: When a route that aborts blocks a request
        let args = json!({"url_pattern": "*/gone/*", "abort": true});
        let reply = mock_route(ts::call("mock_route", args), &handler)
            .await
            .unwrap();
        assert!(
            ts::text(&reply).contains("fail with a network error (NS_ERROR_ABORT)"),
            "{}",
            ts::text(&reply)
        );
        let event = blocked("R3", "http://a/gone/x", "i1");
        mock.bidi().emit("network.beforeRequestSent", event);
        until_sent(&mock, "network.failRequest").await;
        // Then Firefox fails it, and the request log has no status for it
        let failed = &mock.bidi().commands_named("network.failRequest")[0].params;
        assert_eq!(*failed, json!({"request": "R3"}));
        let (_, _, tab) = super::super::tools::target(&handler, &Default::default())
            .await
            .unwrap();
        let network = tab.network.lock().await;
        assert!(network.was_mocked("R1") && !network.was_mocked("R3"));
        drop(network);
        // And a reason other than 'failed' is refused, naming the limit
        let args = json!({"url_pattern": "*", "abort": true, "error_reason": "blocked_by_client"});
        let refused = mock_route(ts::call("mock_route", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("cannot choose why"), "{refused}");
        let _ = remove_route(
            ts::call("remove_route", json!({"route_id": "route-2"})),
            &handler,
        )
        .await;
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

    /// #194 (Firefox): a mocked response Firefox refuses fails the request
    /// instead of leaving it blocked, and list_routes shows Firefox's
    /// error on the route.
    #[tokio::test]
    async fn firefox_fails_a_request_whose_mocked_response_it_refuses() {
        // Given a route whose response Firefox refuses
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        mock.bidi()
            .respond_with("network.addIntercept", |_| json!({"intercept": "i1"}));
        mock.bidi().respond_with("network.provideResponse", |_| {
            json!({"type": "error", "error": "invalid argument", "message": "bad header value"})
        });
        let handler = ts::handler(&mock);
        let args = json!({"url_pattern": "*/api/*", "body": "x"});
        mock_route(ts::call("mock_route", args), &handler)
            .await
            .unwrap();
        // When Firefox blocks a request it matches
        let event = blocked("R1", "http://a/api/x", "i1");
        mock.bidi().emit("network.beforeRequestSent", event);
        until_sent(&mock, "network.failRequest").await;
        // Then the request fails, and the route says why
        let failed = mock.bidi().commands_named("network.failRequest");
        assert_eq!(failed[0].params, json!({"request": "R1"}));
        let reply = list_routes(ts::call("list_routes", json!({})), &handler)
            .await
            .unwrap();
        let route = &ts::json_of(&reply)["routes"][0];
        assert_eq!(route["errors"], 1, "{}", ts::text(&reply));
        assert_eq!(route["last_error"], "invalid argument: bad header value");
        ts::stop(&handler).await;
    }

    /// #195 (Firefox): a request blocked just before remove_route took the
    /// intercept away is let through (Firefox keeps it blocked when its
    /// intercept goes); a route added later answers through a new
    /// intercept, each request once; a request another intercept blocked
    /// is left alone.
    #[tokio::test]
    async fn firefox_lets_through_what_was_blocked_before_remove_route() {
        // Given a tab whose route was removed
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        let added = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let next = added.clone();
        mock.bidi().respond_with("network.addIntercept", move |_| {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json!({"intercept": format!("i{n}")})
        });
        let handler = ts::handler(&mock);
        let route = || {
            let args = json!({"url_pattern": "*/api/*", "body": "m"});
            mock_route(ts::call("mock_route", args), &handler)
        };
        route().await.unwrap();
        remove_route(ts::call("remove_route", json!({})), &handler)
            .await
            .unwrap();
        // When Firefox reports a request its first intercept blocked
        let event = blocked("R1", "http://a/api/x", "i1");
        mock.bidi().emit("network.beforeRequestSent", event);
        until_sent(&mock, "network.continueRequest").await;
        // Then it goes through
        let continued = mock.bidi().commands_named("network.continueRequest");
        assert_eq!(continued.len(), 1);
        assert_eq!(continued[0].params, json!({"request": "R1"}));
        // When a route is added again, and its intercept blocks a request
        route().await.unwrap();
        let event = blocked("R2", "http://a/api/y", "i2");
        mock.bidi().emit("network.beforeRequestSent", event);
        until_sent(&mock, "network.provideResponse").await;
        // And another intercept blocks one
        let event = blocked("R3", "http://a/api/z", "someone-else");
        mock.bidi().emit("network.beforeRequestSent", event);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Then the route answers its request once, and the other is left
        assert_eq!(mock.bidi().count("network.provideResponse"), 1);
        assert_eq!(mock.bidi().count("network.continueRequest"), 1);
        ts::stop(&handler).await;
    }

    /// A route added while the intercept stays but its answering task
    /// ended starts a new task: the requests the intercept blocks are
    /// answered, not left blocked.
    #[tokio::test]
    async fn firefox_restarts_an_answering_task_that_ended() {
        // Given a tab with a route whose answering task has ended
        let mock = bidi_browser_lite::test_support::MockFirefox::start(false)
            .await
            .unwrap();
        mock.bidi()
            .respond_with("network.addIntercept", |_| json!({"intercept": "i1"}));
        let handler = ts::handler(&mock);
        let route = |glob: &str| {
            let args = json!({"url_pattern": glob, "body": "m"});
            mock_route(ts::call("mock_route", args), &handler)
        };
        route("*/a/*").await.unwrap();
        let (_, _, tab) = target(&handler, &ids(&None, &None)).await.unwrap();
        {
            let mut routes = tab.routes.lock().await;
            routes.listener = Default::default();
            routes.listener.push(tokio::spawn(async {}));
        }
        while tab.routes.lock().await.listener.is_running() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // When another route is added and the intercept blocks a request
        route("*/api/*").await.unwrap();
        let event = blocked("R1", "http://a/api/x", "i1");
        mock.bidi().emit("network.beforeRequestSent", event);
        until_sent(&mock, "network.provideResponse").await;
        // Then a new task answers it, through the same intercept
        assert_eq!(mock.bidi().count("network.provideResponse"), 1);
        assert_eq!(mock.bidi().count("network.addIntercept"), 1);
        ts::stop(&handler).await;
    }

    /// A `network.beforeRequestSent` of request `id` to `url`, blocked by
    /// the intercept `intercept`.
    fn blocked(id: &str, url: &str, intercept: &str) -> Value {
        json!({"context": "ctx-0", "isBlocked": true, "intercepts": [intercept], "navigation": null,
            "redirectCount": 0, "timestamp": 0,
            "request": {"request": id, "url": url, "method": "GET", "headers": [], "cookies": [],
                "headersSize": 0, "bodySize": 0, "timings": {}}})
    }

    /// Waits (at most 1 s) until the mock received a `method` command.
    async fn until_sent(mock: &bidi_browser_lite::test_support::MockFirefox, method: &str) {
        for _ in 0..100 {
            if mock.bidi().count(method) > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
