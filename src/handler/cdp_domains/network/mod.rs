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
        "Network.webSocketFrameSent" => on_ws_frame(&mut st, request_id, params, true),
        "Network.webSocketFrameReceived" => on_ws_frame(&mut st, request_id, params, false),
        _ => {}
    }
}

fn on_request(st: &mut NetworkState, request_id: &str, params: &serde_json::Value) {
    let Some(req) = params.get("request") else {
        return;
    };
    st.insert_request(
        request_id.to_string(),
        NetworkRequest {
            url: str_field(req, "url").unwrap_or("").to_string(),
            method: str_field(req, "method").unwrap_or("").to_string(),
            request_headers: req.get("headers").cloned(),
            request_post_data: str_field(req, "postData").map(str::to_string),
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
    req.response_status = res.get("status").and_then(|v| v.as_i64());
    req.response_status_text = str_field(res, "statusText").map(str::to_string);
    req.response_headers = res.get("headers").cloned();
    if req.resource_type.is_none() {
        req.resource_type = str_field(params, "type").map(str::to_string);
    }
    st.apply_extra_headers(request_id);
}

/// Records why a request failed (`errorText`, plus why it was blocked).
fn on_failure(st: &mut NetworkState, request_id: &str, params: &serde_json::Value) {
    let Some(req) = st.find_request_mut(request_id) else {
        return;
    };
    req.finished = true;
    let text = str_field(params, "errorText").unwrap_or("failed");
    req.error = Some(match str_field(params, "blockedReason") {
        Some(reason) => format!("{text} (blocked: {reason})"),
        None => text.to_string(),
    });
}

fn on_ws_frame(st: &mut NetworkState, request_id: &str, params: &serde_json::Value, is_sent: bool) {
    let Some(response) = params.get("response") else {
        return;
    };
    let payload_data = str_field(response, "payloadData").unwrap_or("").to_string();
    let url = st
        .ws_connections
        .get(request_id)
        .cloned()
        .unwrap_or_default();
    st.push_ws_frame(
        request_id.to_string(),
        WebSocketFrame::new(url, payload_data, is_sent),
    );
}

pub(crate) fn start_network_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state: Arc<Mutex<NetworkState>>,
) -> tokio::task::JoinHandle<()> {
    let process = |event, state| async move { process_network_event(&event, &state).await };
    crate::handler::cdp_domains::event_pump::spawn_state_listener(target, "Network", state, process)
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
        assert_eq!(st.ws_connections.get("ws-1").unwrap(), "wss://socket.com");
        let frames = st.ws_frames.get("ws-1").unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_sent);
        assert_eq!(frames[0].payload_data, "ping");
        assert!(!frames[1].is_sent);
        assert_eq!(frames[1].payload_data, "pong");
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

    /// FIX-05: A redirect keeps one entry per request id with the redirect chain.
    #[tokio::test]
    async fn redirects_keep_one_entry() {
        // Given requestWillBeSent twice for "R1", the second carrying the redirect
        let state = Arc::new(Mutex::new(NetworkState::default()));
        let first = json!({"requestId": "R1", "request": {"url": "http://a/old", "method": "GET"}});
        let second = json!({"requestId": "R1", "request": {"url": "http://a/new", "method": "GET"},
            "redirectResponse": {"status": 302}});
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
        assert_eq!(all["R1"].redirects, vec!["http://a/old".to_string()]);
    }
}
