pub mod get_network_logs;
pub mod set_offline;

use crate::handler::NetworkRequest;
use crate::handler::NetworkState;
use crate::handler::WebSocketFrame;
use crate::util::json::str_field;
use cdp_browser_lite::WsResponse;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn process_network_event(event: &WsResponse, state: &Arc<Mutex<NetworkState>>) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    let Some(request_id) = str_field(params, "requestId") else {
        return;
    };
    let mut st = state.lock().await;
    match method {
        "Network.requestWillBeSent" => on_request(&mut st, request_id, params),
        "Network.responseReceived" => on_response(&mut st, request_id, params),
        "Network.loadingFailed" => on_failure(&mut st, request_id, params),
        "Network.loadingFinished" => {
            if let Some(req) = st.find_request_mut(request_id) {
                req.finished = true;
            }
        }
        "Network.requestWillBeSentExtraInfo" | "Network.responseReceivedExtraInfo" => {
            let response = method == "Network.responseReceivedExtraInfo";
            let headers = params.get("headers").cloned().unwrap_or_default();
            st.note_extra_headers(request_id, response, headers);
        }
        "Network.webSocketCreated" => {
            let url = str_field(params, "url").unwrap_or("").to_string();
            st.insert_ws_connection(request_id.to_string(), url);
        }
        // #153: a socket that closed or failed looked open.
        "Network.webSocketClosed" => {
            let close = crate::handler::WsClose {
                at: crate::handler::now_ms(),
                ..Default::default()
            };
            st.close_ws_connection(request_id, close);
        }
        "Network.webSocketFrameError" => {
            let error = str_field(params, "errorMessage").unwrap_or("frame error");
            st.fail_ws_connection(request_id, error);
        }
        "Network.webSocketFrameSent" => on_ws_frame(&mut st, request_id, params, true),
        "Network.webSocketFrameReceived" => on_ws_frame(&mut st, request_id, params, false),
        _ => {}
    }
}

fn on_request(st: &mut NetworkState, request_id: &str, params: &serde_json::Value) {
    let Some(req) = params.get("request") else {
        return;
    };
    // The same event from a second session (a popup's, #141): not a hop.
    let sent_at = params.get("timestamp").and_then(serde_json::Value::as_f64);
    if sent_at.is_some()
        && st
            .find_request_mut(request_id)
            .is_some_and(|known| known.sent_at == sent_at)
    {
        return;
    }
    // A redirect: the hop's response comes with the next request only.
    if let Some(hop) = params.get("redirectResponse")
        && let Some(previous) = st.find_request_mut(request_id)
    {
        answer(previous, hop);
    }
    st.insert_request(
        request_id.to_string(),
        NetworkRequest {
            url: str_field(req, "url").unwrap_or("").to_string(),
            method: str_field(req, "method").unwrap_or("").to_string(),
            request_headers: req.get("headers").cloned(),
            request_post_data: str_field(req, "postData").map(str::to_string),
            sent_at,
            ..Default::default()
        },
    );
    st.apply_extra_headers(request_id);
}

fn on_response(st: &mut NetworkState, request_id: &str, params: &serde_json::Value) {
    let Some(res) = params.get("response") else {
        return;
    };
    let Some(req) = st.find_request_mut(request_id) else {
        return;
    };
    answer(req, res);
    if req.resource_type.is_none() {
        req.resource_type = str_field(params, "type").map(str::to_string);
    }
    st.apply_extra_headers(request_id);
}

/// Records a CDP `Response`'s status and headers on `req`.
fn answer(req: &mut NetworkRequest, response: &serde_json::Value) {
    req.response_status = response.get("status").and_then(serde_json::Value::as_i64);
    req.response_status_text = str_field(response, "statusText").map(str::to_string);
    req.response_headers = response.get("headers").cloned();
}

/// Records why a request failed (`errorText`, plus why it was blocked or
/// which CORS check failed). One the page canceled after its response
/// arrived (an `EventSource` it closed) did not fail: it keeps its status.
fn on_failure(st: &mut NetworkState, request_id: &str, params: &serde_json::Value) {
    let Some(req) = st.find_request_mut(request_id) else {
        return;
    };
    req.finished = true;
    let canceled = params.get("canceled").and_then(serde_json::Value::as_bool) == Some(true);
    if canceled && req.response_status.is_some() {
        req.canceled_by_page = true;
        return;
    }
    let text = str_field(params, "errorText").unwrap_or("failed");
    let cors = params
        .get("corsErrorStatus")
        .and_then(|status| str_field(status, "corsError"));
    req.error = Some(match (str_field(params, "blockedReason"), cors) {
        (_, Some(cors)) => format!("{text} (blocked by CORS: {cors})"),
        (Some(reason), None) => format!("{text} (blocked: {reason})"),
        (None, None) => text.to_string(),
    });
}

fn on_ws_frame(st: &mut NetworkState, request_id: &str, params: &serde_json::Value, is_sent: bool) {
    let Some(response) = params.get("response") else {
        return;
    };
    let payload_data = str_field(response, "payloadData").unwrap_or("").to_string();
    let url = st.ws_url(request_id);
    st.push_ws_frame(
        request_id.to_string(),
        WebSocketFrame::new(url, payload_data, is_sent),
    );
}

/// Why a WebSocket connection is closed when its document goes away.
pub(crate) const NAVIGATED_AWAY: &str = "the page navigated away";

/// Why the sockets of a frame that went away are closed (#163, #167).
pub(crate) const FRAME_GONE: &str = "the frame was removed or navigated away";

/// [`process_network_event`] for an event of the tab's whole stream: a
/// main-frame navigation also closes the WebSocket connections the page
/// it replaced left open, as Chrome reports no close for them (#154).
async fn process_tab_event(event: &WsResponse, state: &Arc<Mutex<NetworkState>>) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    if crate::handler::cdp_domains::log::main_frame_commit(method, params).is_some() {
        state.lock().await.close_open_ws_connections(NAVIGATED_AWAY);
    } else if method.starts_with("Network.") {
        process_network_event(event, state).await;
    }
}

/// Feeds the tab's network state. One ordered stream of every domain
/// (filtered in [`process_tab_event`]): a navigation must close the old
/// page's sockets before the new page's open, which separate per-domain
/// tasks would not guarantee.
pub(crate) fn start_network_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<NetworkState>>,
) -> tokio::task::JoinHandle<()> {
    let process = |event, state| async move { process_tab_event(&event, &state).await };
    crate::handler::cdp_domains::event_pump::spawn_state_listener(target, "", state, process)
}

/// [`start_network_listener`] for a frame of another origin (or a worker)
/// on Chrome, whose session is `session`: its requests note it, so their
/// bodies are asked there. Any event of a request counts: the frame's own
/// document is requested by its parent, before the frame's session exists,
/// and only its response reaches the frame's session. Its WebSockets note
/// `frames`, the targets of the frames of another origin it lives in, to
/// be closed when one of them goes (#167).
pub(crate) fn start_frame_network_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<NetworkState>>,
    session: String,
    frames: Vec<String>,
) -> tokio::task::JoinHandle<()> {
    let frames = Arc::new(frames);
    let process = move |event, state| {
        process_placed_event(event, state, session.clone(), Arc::clone(&frames))
    };
    crate::handler::cdp_domains::event_pump::spawn_state_listener(target, "Network", state, process)
}

/// [`process_frame_event`], then a socket the frame opened notes the
/// frames it lives in.
async fn process_placed_event(
    event: WsResponse,
    state: Arc<Mutex<NetworkState>>,
    session: String,
    frames: Arc<Vec<String>>,
) {
    let created = (event.method.as_deref() == Some("Network.webSocketCreated"))
        .then(|| {
            event
                .params
                .as_ref()
                .and_then(|p| str_field(p, "requestId"))
        })
        .flatten()
        .map(str::to_string);
    process_frame_event(event, Arc::clone(&state), session).await;
    if let Some(id) = created {
        state.lock().await.place_ws_connection(&id, &frames);
    }
}

/// [`process_network_event`] for an event of the frame (or the popup's
/// auto-attached session, see `page_attach`) whose session is `session`,
/// then notes that session on the request.
pub(crate) async fn process_frame_event(
    event: WsResponse,
    state: Arc<Mutex<NetworkState>>,
    session: String,
) {
    process_network_event(&event, &state).await;
    let id = event
        .params
        .as_ref()
        .and_then(|p| str_field(p, "requestId"));
    if let Some(id) = id
        && let Some(request) = state.lock().await.find_request_mut(id)
    {
        request.session = Some(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::make_event;
    use serde_json::json;

    #[tokio::test]
    async fn test_request_will_be_sent() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let event = make_event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "req-1",
                "request": {
                    "url": "https://example.com/api",
                    "method": "POST",
                    "headers": { "Content-Type": "application/json" },
                    "postData": "{\"foo\": \"bar\"}"
                }
            }),
        );

        process_network_event(&event, &state).await;

        let st = state.lock().await;
        let reqs = st.iter_all();
        let req = reqs.get("req-1").unwrap();
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.method, "POST");
        assert_eq!(req.request_post_data.as_deref(), Some("{\"foo\": \"bar\"}"));
    }

    #[tokio::test]
    async fn test_response_received() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        // Pre-insert request
        {
            let mut st = state.lock().await;
            st.insert_request(
                "req-1".to_string(),
                NetworkRequest {
                    url: "https://example.com".into(),
                    method: "GET".into(),
                    ..Default::default()
                },
            );
        }

        let event = make_event(
            "Network.responseReceived",
            json!({
                "requestId": "req-1",
                "type": "XHR",
                "response": {
                    "status": 200,
                    "statusText": "OK",
                    "headers": { "X-Test": "val" }
                }
            }),
        );

        process_network_event(&event, &state).await;

        let mut st = state.lock().await;
        let req = st.find_request_mut("req-1").unwrap();
        assert_eq!(req.response_status, Some(200));
        assert_eq!(req.response_status_text.as_deref(), Some("OK"));
        assert_eq!(req.resource_type.as_deref(), Some("XHR"));
    }

    #[tokio::test]
    async fn test_websocket_lifecycle() {
        let state = Arc::new(Mutex::new(NetworkState::default()));

        // 1. Created
        let event_created = make_event(
            "Network.webSocketCreated",
            json!({
                "requestId": "ws-1",
                "url": "wss://socket.com"
            }),
        );
        process_network_event(&event_created, &state).await;

        // 2. Frame Sent
        let event_sent = make_event(
            "Network.webSocketFrameSent",
            json!({
                "requestId": "ws-1",
                "response": { "payloadData": "ping" }
            }),
        );
        process_network_event(&event_sent, &state).await;

        // 3. Frame Received
        let event_received = make_event(
            "Network.webSocketFrameReceived",
            json!({
                "requestId": "ws-1",
                "response": { "payloadData": "pong" }
            }),
        );
        process_network_event(&event_received, &state).await;

        let st = state.lock().await;
        assert_eq!(st.ws_url("ws-1"), "wss://socket.com");
        let frames = st.ws_frames.get("ws-1").unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_sent);
        assert_eq!(frames[0].payload_data, "ping");
        assert!(!frames[1].is_sent);
        assert_eq!(frames[1].payload_data, "pong");
    }

    /// #153: `webSocketClosed` closes the connection, once (the popup's
    /// session and the tab's both deliver it); `webSocketFrameError`
    /// records why a frame failed.
    #[tokio::test]
    async fn websocket_closes_and_errors_are_recorded() {
        // Given two connections
        let state = Arc::new(Mutex::new(NetworkState::default()));
        for (id, url) in [("ws-1", "wss://a/feed"), ("ws-2", "wss://b/chat")] {
            let created = json!({"requestId": id, "url": url});
            process_network_event(&make_event("Network.webSocketCreated", created), &state).await;
        }
        // When the first closes, twice, and the second has a frame error
        let closed = json!({"requestId": "ws-1", "timestamp": 12.5});
        process_network_event(
            &make_event("Network.webSocketClosed", closed.clone()),
            &state,
        )
        .await;
        let first = state.lock().await.ws_connections["ws-1"].closed_at;
        process_network_event(&make_event("Network.webSocketClosed", closed), &state).await;
        let error =
            json!({"requestId": "ws-2", "timestamp": 13.0, "errorMessage": "Invalid frame header"});
        process_network_event(&make_event("Network.webSocketFrameError", error), &state).await;
        // Then the first keeps its first close time, and the second its error
        let st = state.lock().await;
        assert!(first.is_some());
        assert_eq!(st.ws_connections["ws-1"].closed_at, first);
        assert_eq!(st.ws_connections["ws-1"].close_code, None);
        let failed = &st.ws_connections["ws-2"];
        assert_eq!(failed.error.as_deref(), Some("Invalid frame header"));
        assert_eq!(failed.closed_at, None);
        // And a duplicate webSocketCreated does not reopen a closed one
        drop(st);
        let created = json!({"requestId": "ws-1", "url": "wss://a/feed"});
        process_network_event(&make_event("Network.webSocketCreated", created), &state).await;
        assert_eq!(state.lock().await.ws_connections["ws-1"].closed_at, first);
    }

    /// #154: a main-frame navigation closes the WebSocket connections of
    /// the document it replaced (Chrome reports no close for them); a
    /// frame's navigation does not, nor are the next document's sockets,
    /// created after it, closed.
    #[tokio::test]
    async fn main_frame_navigations_close_open_websockets() {
        // Given an open connection and a closed one
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let event = |method: &str, params: serde_json::Value| make_event(method, params);
        for id in ["ws-1", "ws-2"] {
            let created = json!({"requestId": id, "url": format!("ws://a/{id}")});
            process_tab_event(&event("Network.webSocketCreated", created), &state).await;
        }
        let closed = json!({"requestId": "ws-2", "timestamp": 1.0});
        process_tab_event(&event("Network.webSocketClosed", closed), &state).await;
        let before = state.lock().await.ws_connections["ws-2"].clone();
        // When a frame navigates, then the main frame, then a new socket opens
        let frame = json!({"frame": {"id": "F2", "parentId": "F1", "url": "http://a/f"}});
        process_tab_event(&event("Page.frameNavigated", frame), &state).await;
        assert!(
            state.lock().await.ws_connections["ws-1"]
                .closed_at
                .is_none()
        );
        let main = json!({"frame": {"id": "F1", "url": "http://a/next"}, "type": "Navigation"});
        process_tab_event(&event("Page.frameNavigated", main), &state).await;
        let created = json!({"requestId": "ws-3", "url": "ws://a/ws-3"});
        process_tab_event(&event("Network.webSocketCreated", created), &state).await;
        // Then the old document's open socket is closed, saying why
        let st = state.lock().await;
        let gone = &st.ws_connections["ws-1"];
        assert!(gone.closed_at.is_some());
        assert_eq!(gone.close_reason.as_deref(), Some(NAVIGATED_AWAY));
        // And the one that had closed keeps its close, the new one is open
        assert_eq!(st.ws_connections["ws-2"], before);
        assert!(!st.ws_connections["ws-3"].ended());
    }

    /// AUD-OB-17: the headers Chrome only reports in the ExtraInfo events
    /// (Cookie, Set-Cookie) reach the request, whichever event comes first.
    #[tokio::test]
    async fn extra_info_headers_are_merged() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let event = |method: &str, params: serde_json::Value| make_event(method, params);
        // Request ExtraInfo before the request, response ExtraInfo before the response
        let extra = json!({"requestId": "r1", "headers": {"Cookie": "s=1", "Host": "a"}});
        process_network_event(&event("Network.requestWillBeSentExtraInfo", extra), &state).await;
        let sent = json!({"requestId": "r1", "request": {"url": "http://a/", "method": "GET", "headers": {"Accept": "*/*"}}});
        process_network_event(&event("Network.requestWillBeSent", sent), &state).await;
        let extra = json!({"requestId": "r1", "headers": {"set-cookie": "s=2\nt=3"}});
        process_network_event(&event("Network.responseReceivedExtraInfo", extra), &state).await;
        let answered = json!({"requestId": "r1", "response": {"status": 200, "headers": {"Content-Type": "text/html", "Set-Cookie": "s=2"}}});
        process_network_event(&event("Network.responseReceived", answered), &state).await;
        let mut st = state.lock().await;
        let req = st.find_request_mut("r1").unwrap();
        assert_eq!(
            req.request_headers,
            Some(json!({"Accept": "*/*", "Cookie": "s=1", "Host": "a"}))
        );
        assert_eq!(
            req.response_headers,
            Some(json!({"Content-Type": "text/html", "set-cookie": "s=2\nt=3"}))
        );
        drop(st);
        // And an ExtraInfo arriving after its request is merged too
        let sent = json!({"requestId": "r2", "request": {"url": "http://a/x", "method": "GET", "headers": {}}});
        process_network_event(&event("Network.requestWillBeSent", sent), &state).await;
        let extra = json!({"requestId": "r2", "headers": {"Cookie": "late=1"}});
        process_network_event(&event("Network.requestWillBeSentExtraInfo", extra), &state).await;
        let mut st = state.lock().await;
        let headers = st.find_request_mut("r2").unwrap().request_headers.clone();
        assert_eq!(headers, Some(json!({"Cookie": "late=1"})));
    }

    /// AUD-OB-14: a request is finished once Chrome says it loaded (or
    /// failed), not when its headers arrive.
    #[tokio::test]
    async fn loading_finished_marks_the_request() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let sent = json!({"requestId": "r1", "request": {"url": "http://a/", "method": "GET", "headers": {}}});
        process_network_event(&make_event("Network.requestWillBeSent", sent), &state).await;
        let answered = json!({"requestId": "r1", "response": {"status": 200, "headers": {}}});
        process_network_event(&make_event("Network.responseReceived", answered), &state).await;
        assert!(!state.lock().await.find_request_mut("r1").unwrap().finished);
        let done = json!({"requestId": "r1", "encodedDataLength": 10});
        process_network_event(&make_event("Network.loadingFinished", done), &state).await;
        assert!(state.lock().await.find_request_mut("r1").unwrap().finished);
    }

    /// AUD-OB-07: a failed request says why (it looked like one still
    /// waiting: status and text null).
    #[tokio::test]
    async fn failed_request_records_its_error() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let sent = json!({"requestId": "r1", "request": {"url": "http://nope.invalid/", "method": "GET", "headers": {}}});
        process_network_event(&make_event("Network.requestWillBeSent", sent), &state).await;
        let failed = json!({"requestId": "r1", "errorText": "net::ERR_NAME_NOT_RESOLVED", "canceled": false});
        process_network_event(&make_event("Network.loadingFailed", failed), &state).await;
        let blocked = json!({"requestId": "r1", "errorText": "net::ERR_BLOCKED_BY_CLIENT", "blockedReason": "inspector"});
        let mut st = state.lock().await;
        assert_eq!(
            st.find_request_mut("r1").unwrap().error.as_deref(),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
        drop(st);
        process_network_event(&make_event("Network.loadingFailed", blocked), &state).await;
        let mut st = state.lock().await;
        assert_eq!(
            st.find_request_mut("r1").unwrap().error.as_deref(),
            Some("net::ERR_BLOCKED_BY_CLIENT (blocked: inspector)")
        );
    }

    /// #141: the same `requestWillBeSent` received from two sessions (a
    /// popup's auto-attached one and its tab's) is one request, not a
    /// redirect; a real hop (a later timestamp) still is.
    #[tokio::test]
    async fn the_same_request_from_two_sessions_is_not_a_hop() {
        // Given a request sent, seen twice, then redirected
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let sent = json!({"requestId": "R1", "timestamp": 10.5,
            "request": {"url": "http://a/old", "method": "GET"}});
        let hop = json!({"requestId": "R1", "timestamp": 10.7,
            "request": {"url": "http://a/new", "method": "GET"},
            "redirectResponse": {"status": 302, "headers": {"location": "/new"}}});
        // When each event arrives twice
        for params in [&sent, &sent, &hop, &hop] {
            let event = make_event("Network.requestWillBeSent", params.clone());
            process_network_event(&event, &state).await;
        }
        // Then R1 has one hop, to its final URL
        let mut st = state.lock().await;
        let request = st.find_request_mut("R1").unwrap();
        assert_eq!(request.url, "http://a/new");
        assert_eq!(request.redirects.len(), 1);
    }

    /// FIX-05: A redirect keeps one entry per request id with the redirect chain.
    #[tokio::test]
    async fn redirects_keep_one_entry() {
        // Given requestWillBeSent twice for "R1", the second carrying the redirect
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let first = json!({"requestId": "R1", "request": {"url": "http://a/old", "method": "GET"}});
        let second = json!({"requestId": "R1", "request": {"url": "http://a/new", "method": "GET"},
            "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"location": "/new"}}});
        for params in [first, second] {
            process_network_event(&make_event("Network.requestWillBeSent", params), &state).await;
        }
        // Then R1 is listed once, with the final URL and one redirect
        let st = state.lock().await;
        let all = st.iter_all();
        assert_eq!(all.len(), 1);
        assert_eq!(
            st.navigations[0].order.len(),
            1,
            "id must not be duplicated"
        );
        assert_eq!(all["R1"].url, "http://a/new");
        // ISSUE-78: each hop keeps its status and Location
        let hop = crate::handler::RedirectHop {
            url: "http://a/old".into(),
            status: Some(302),
            location: Some("/new".into()),
        };
        assert_eq!(all["R1"].redirects, vec![hop]);
        assert_eq!(
            all["R1"].response_status, None,
            "the final response is not in yet"
        );
    }

    /// ISSUE-78: a request blocked by CORS says why (`corsErrorStatus`),
    /// not only `net::ERR_FAILED`.
    #[tokio::test]
    async fn cors_failures_name_the_reason() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let sent = json!({"requestId": "r1", "request": {"url": "http://b/api", "method": "GET", "headers": {}}});
        process_network_event(&make_event("Network.requestWillBeSent", sent), &state).await;
        let failed = json!({"requestId": "r1", "errorText": "net::ERR_FAILED", "canceled": false,
            "corsErrorStatus": {"corsError": "MissingAllowOriginHeader", "failedParameter": ""}});
        process_network_event(&make_event("Network.loadingFailed", failed), &state).await;
        let mut st = state.lock().await;
        assert_eq!(
            st.find_request_mut("r1").unwrap().error.as_deref(),
            Some("net::ERR_FAILED (blocked by CORS: MissingAllowOriginHeader)")
        );
    }

    /// ISSUE-78: a response the page canceled after it arrived (an
    /// `EventSource` it closed) keeps its status and says it was canceled,
    /// instead of reading as failed; one canceled before any response
    /// still fails.
    #[tokio::test]
    async fn requests_canceled_after_their_response_keep_the_status() {
        let state = Arc::new(Mutex::new(NetworkState::default()));
        for id in ["sse", "early"] {
            let sent = json!({"requestId": id, "request": {"url": "http://a/sse", "method": "GET", "headers": {}}});
            process_network_event(&make_event("Network.requestWillBeSent", sent), &state).await;
        }
        let answered = json!({"requestId": "sse", "response": {"status": 200, "headers": {}}});
        process_network_event(&make_event("Network.responseReceived", answered), &state).await;
        for id in ["sse", "early"] {
            let canceled =
                json!({"requestId": id, "errorText": "net::ERR_ABORTED", "canceled": true});
            process_network_event(&make_event("Network.loadingFailed", canceled), &state).await;
        }
        let mut st = state.lock().await;
        let sse = st.find_request_mut("sse").unwrap().clone();
        assert_eq!(
            (
                sse.error,
                sse.response_status,
                sse.canceled_by_page,
                sse.finished
            ),
            (None, Some(200), true, true)
        );
        let early = st.find_request_mut("early").unwrap().clone();
        assert_eq!(early.error.as_deref(), Some("net::ERR_ABORTED"));
        assert!(!early.canceled_by_page);
    }
}
