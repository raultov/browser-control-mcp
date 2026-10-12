use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::{McpHandler, NetworkRequest, WebSocketFrame};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};

#[macros::mcp_tool(
    name = "get_network_logs",
    description = "Retrieves intercepted HTTP/REST requests and WebSocket frames from network activity cache with filtering. Side effects: when 'clear' is true the cached requests and WebSocket frames are emptied after being returned. Prerequisites: requires active Chrome tab with network monitoring enabled. Returns: JSON array of requests/WebSocket frames with optional full details ('include_details' defaults to true; set false for summary only). Parameters: 'current_navigation_only' limits results to current navigation only (defaults to false, retaining up to 3 navigations). Rate limits: none. Use this to audit API calls, debug network issues, inspect WebSocket traffic. Alternatives: browser DevTools Network tab, HAR file export."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetNetworkLogsTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Clear network cache after returning logs. Constraints: boolean. Interactions: when true, subsequent calls return only new traffic. Defaults to: false.
    #[serde(default)]
    pub clear: Option<bool>,

    /// Traffic type to include. Constraints: 'rest', 'websocket', or 'both' (case-insensitive). Interactions: limits results to specified type. Defaults to: "both".
    #[serde(default)]
    pub type_filter: Option<String>,

    /// Partial URL match (case-insensitive). Constraints: non-empty string. Interactions: filters both REST and WebSocket URLs; empty string disables filtering. Defaults to: None (no URL filtering).
    #[serde(default)]
    pub url_filter: Option<String>,

    /// WebSocket frame direction filter. Constraints: 'sent', 'received', or 'both'. Interactions: applies only when type_filter includes 'websocket'. Defaults to: "both".
    #[serde(default)]
    pub ws_direction_filter: Option<String>,

    /// WebSocket payload substring match (case-insensitive). Constraints: non-empty string. Interactions: applies only when type_filter includes 'websocket'; filters by payload content. Defaults to: None (no content filtering).
    #[serde(default)]
    pub ws_content_filter: Option<String>,

    /// Include full request/response details. Constraints: boolean. Interactions: when false, returns summary only (URL, method, status); when true, includes headers, bodies. Defaults to: true.
    #[serde(default)]
    pub include_details: Option<bool>,

    /// Return the values of sensitive headers (Authorization, Proxy-Authorization, Cookie, Set-Cookie) instead of `"<redacted>"`. Constraints: boolean. Interactions: only meaningful with include_details. Defaults to: false.
    #[serde(default)]
    pub include_sensitive_headers: Option<bool>,

    /// Limit results to current navigation only. Constraints: boolean. Interactions: when false, includes up to 3 retained navigations. Defaults to: false.
    #[serde(default)]
    pub current_navigation_only: Option<bool>,
}

type Requests = HashMap<String, NetworkRequest>;
type Frames = HashMap<String, VecDeque<WebSocketFrame>>;

/// The parsed filters of one call, all lowercase.
struct Filters {
    want_rest: bool,
    want_ws: bool,
    url: String,
    ws_direction: String,
    ws_content: String,
    include_details: bool,
}

impl Filters {
    fn new(args: &GetNetworkLogsTool) -> Self {
        let lower =
            |v: &Option<String>, default: &str| v.as_deref().unwrap_or(default).to_lowercase();
        let type_filter = lower(&args.type_filter, "both");
        Self {
            want_rest: type_filter == "rest" || type_filter == "both",
            want_ws: type_filter == "websocket" || type_filter == "both",
            url: lower(&args.url_filter, ""),
            ws_direction: lower(&args.ws_direction_filter, "both"),
            ws_content: lower(&args.ws_content_filter, ""),
            include_details: args.include_details.unwrap_or(true),
        }
    }

    fn url_matches(&self, url: &str) -> bool {
        self.url.is_empty() || url.to_lowercase().contains(&self.url)
    }

    fn frame_matches(&self, frame: &WebSocketFrame) -> bool {
        let direction_ok = match self.ws_direction.as_str() {
            "sent" => frame.is_sent,
            "received" => !frame.is_sent,
            _ => true,
        };
        direction_ok
            && self.url_matches(&frame.url)
            && (self.ws_content.is_empty()
                || frame.payload_data.to_lowercase().contains(&self.ws_content))
    }
}

/// A filtered copy of a tab's network buffers.
pub(crate) struct NetworkSnapshot {
    pub(crate) requests: Requests,
    pub(crate) frames: Frames,
    /// Ids of the current navigation's requests.
    pub(crate) current_ids: HashSet<String>,
}

impl GetNetworkLogsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: GetNetworkLogsTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let network_state = session.network_state(args.tab_id.clone())?;
        let mut snapshot = args.collect(&network_state).await;
        // `.ok()` first: the error type is not `Send` across the next await.
        let target = match args.wants_bodies() {
            true => session.target(args.tab_id.clone()).await.ok(),
            false => None,
        };
        if let Some(target) = target {
            fetch_bodies(&target, &mut snapshot.requests, &snapshot.current_ids).await;
        }
        Ok(args.finish(snapshot, Vec::new()))
    }

    /// Whether response bodies should be fetched (REST requests with details).
    pub(crate) fn wants_bodies(&self) -> bool {
        let filters = Filters::new(self);
        filters.want_rest && filters.include_details
    }

    /// Copies the tab's buffers (clearing them if asked) and applies the
    /// type, URL, direction and content filters. Shared by every browser.
    pub(crate) async fn collect(
        &self,
        network_state: &std::sync::Arc<tokio::sync::Mutex<crate::handler::NetworkState>>,
    ) -> NetworkSnapshot {
        let filters = Filters::new(self);
        let mut snapshot = {
            let mut st = network_state.lock().await;
            let requests = if self.current_navigation_only.unwrap_or(false) {
                st.iter_current()
            } else {
                st.iter_all()
            };
            let snapshot = NetworkSnapshot {
                requests,
                frames: st.ws_frames.clone().into_iter().collect(),
                current_ids: st.current_ids(),
            };
            if self.clear.unwrap_or(false) {
                st.clear_all();
            }
            snapshot
        };
        if filters.want_rest {
            snapshot
                .requests
                .retain(|_, req| filters.url_matches(&req.url));
        } else {
            snapshot.requests.clear();
        }
        if filters.want_ws {
            snapshot.frames.retain(|_, frames| {
                frames.retain(|f| filters.frame_matches(f));
                !frames.is_empty()
            });
        } else {
            snapshot.frames.clear();
        }
        snapshot
    }

    /// The tool's output: redacted unless asked otherwise; `notes` (browser
    /// limits, lost events) are listed under `notes`.
    pub(crate) fn finish(
        &self,
        mut snapshot: NetworkSnapshot,
        notes: Vec<String>,
    ) -> CallToolResult {
        let filters = Filters::new(self);
        if filters.include_details && !self.include_sensitive_headers.unwrap_or(false) {
            redact_requests(&mut snapshot.requests);
        }
        let mut output = serde_json::Map::new();
        if filters.want_rest {
            let requests = requests_json(snapshot.requests, filters.include_details);
            output.insert("requests".into(), requests);
        }
        if filters.want_ws {
            let frames = frames_json(snapshot.frames, filters.include_details);
            output.insert("websocket_frames".into(), frames);
        }
        if !notes.is_empty() {
            output.insert("notes".into(), json!(notes));
        }
        CallToolResult::text_content(vec![
            serde_json::to_string_pretty(&output)
                .unwrap_or_default()
                .into(),
        ])
    }
}

/// `Network.getResponseBody` for answered requests of the current navigation
/// (bodies are only kept in the returned copy).
async fn fetch_bodies(target: &CdpTarget, requests: &mut Requests, current: &HashSet<String>) {
    for (req_id, req) in requests.iter_mut() {
        if !current.contains(req_id) || req.response_status.is_none() || req.response_body.is_some()
        {
            continue;
        }
        let reply = target
            .send_raw_command("Network.getResponseBody", json!({"requestId": req_id}))
            .await;
        if let Ok(reply) = reply
            && let Some(body) = reply.result.as_ref().and_then(|r| r.get("body"))
        {
            req.response_body = body.as_str().map(str::to_string);
        }
    }
}

fn requests_json(requests: Requests, include_details: bool) -> serde_json::Value {
    if include_details {
        return serde_json::to_value(&requests).unwrap_or_default();
    }
    let summary = requests.into_iter().map(|(id, req)| {
        let row = json!({
            "url": req.url,
            "method": req.method,
            "status": req.response_status,
            "statusText": req.response_status_text,
            "resourceType": req.resource_type
        });
        (id, row)
    });
    serde_json::Value::Object(summary.collect())
}

fn frames_json(frames: Frames, include_details: bool) -> serde_json::Value {
    if include_details {
        return serde_json::to_value(&frames).unwrap_or_default();
    }
    let summary = frames.into_iter().map(|(id, frames)| {
        let rows: Vec<_> = frames
            .into_iter()
            .map(|f| json!({"url": f.url, "is_sent": f.is_sent, "payload_len": f.payload_data.len()}))
            .collect();
        (id, serde_json::Value::Array(rows))
    });
    serde_json::Value::Object(summary.collect())
}

/// Hides the values of credential-carrying headers in every request.
fn redact_requests(requests: &mut Requests) {
    for request in requests.values_mut() {
        for headers in [&mut request.request_headers, &mut request.response_headers]
            .into_iter()
            .flatten()
        {
            crate::util::headers::redact_sensitive(headers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{NetworkRequest, WebSocketFrame};
    use rust_mcp_sdk::schema::ContentBlock;

    trait ContentBlockExt {
        fn as_text(&self) -> Option<&str>;
    }

    impl ContentBlockExt for ContentBlock {
        fn as_text(&self) -> Option<&str> {
            match self {
                ContentBlock::TextContent(t) => Some(&t.text),
                _ => None,
            }
        }
    }

    async fn setup_mock_data(handler: &McpHandler) {
        let mut st = handler.default_session.network_state.lock().await;

        // Mock REST request
        st.insert_request(
            "req-1".to_string(),
            NetworkRequest {
                url: "https://example.com/api/v1".to_string(),
                method: "GET".to_string(),
                resource_type: Some("XHR".to_string()),
                request_headers: None,
                request_post_data: None,
                response_status: Some(200),
                response_status_text: Some("OK".to_string()),
                response_headers: None,
                response_body: None,
                redirects: Vec::new(),
            },
        );

        st.insert_request(
            "req-2".to_string(),
            NetworkRequest {
                url: "https://google.com/search".to_string(),
                method: "GET".to_string(),
                resource_type: Some("Document".to_string()),
                request_headers: None,
                request_post_data: None,
                response_status: Some(200),
                response_status_text: Some("OK".to_string()),
                response_headers: None,
                response_body: None,
                redirects: Vec::new(),
            },
        );

        // Mock WS frames
        st.push_ws_frame(
            "ws-1".to_string(),
            WebSocketFrame {
                url: "wss://socket.com/feed".to_string(),
                payload_data: "hello server".to_string(),
                is_sent: true,
            },
        );
        st.push_ws_frame(
            "ws-1".to_string(),
            WebSocketFrame {
                url: "wss://socket.com/feed".to_string(),
                payload_data: "welcome client".to_string(),
                is_sent: false,
            },
        );
    }

    #[tokio::test]
    async fn test_get_network_logs_filter_type_rest() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "type_filter": "rest"
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        assert!(json.get("requests").is_some());
        assert!(json.get("websocket_frames").is_none());
        assert_eq!(json["requests"].as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_get_network_logs_filter_type_websocket() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "type_filter": "websocket"
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        assert!(json.get("requests").is_none());
        assert!(json.get("websocket_frames").is_some());
        assert_eq!(json["websocket_frames"].as_object().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_get_network_logs_filter_url() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "url_filter": "example.com"
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        assert_eq!(json["requests"].as_object().unwrap().len(), 1);
        assert!(json["requests"].as_object().unwrap().contains_key("req-1"));
        assert_eq!(json["websocket_frames"].as_object().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_get_network_logs_ws_direction_sent() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "type_filter": "websocket",
                "ws_direction_filter": "sent"
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        let frames = &json["websocket_frames"]["ws-1"];
        assert_eq!(frames.as_array().unwrap().len(), 1);
        assert!(frames[0]["is_sent"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_get_network_logs_ws_content_filter() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "ws_content_filter": "welcome"
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        let frames = &json["websocket_frames"]["ws-1"];
        assert_eq!(frames.as_array().unwrap().len(), 1);
        assert!(
            frames[0]["payload_data"]
                .as_str()
                .unwrap()
                .contains("welcome")
        );
    }

    #[tokio::test]
    async fn test_get_network_logs_include_details_false() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": false
            }
        }))
        .unwrap();

        let result = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(result.content[0].as_text().unwrap()).unwrap();

        // Check REST summary
        let req = &json["requests"]["req-1"];
        assert!(req.get("url").is_some());
        assert!(req.get("request_headers").is_none());
        assert!(req.get("statusText").is_some());

        // Check WS summary
        let frames = &json["websocket_frames"]["ws-1"];
        assert!(frames[0].get("payload_len").is_some());
        assert!(frames[0].get("payload_data").is_none());
    }

    #[tokio::test]
    async fn test_get_network_logs_clear() {
        let handler = McpHandler::new_test();
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "clear": true
            }
        }))
        .unwrap();

        // First call should return data
        let result1 = GetNetworkLogsTool::handle(params.clone(), &handler)
            .await
            .unwrap();
        let json1: serde_json::Value =
            serde_json::from_str(result1.content[0].as_text().unwrap()).unwrap();
        assert!(!json1["requests"].as_object().unwrap().is_empty());

        // Second call should be empty
        let result2 = GetNetworkLogsTool::handle(params, &handler).await.unwrap();
        let json2: serde_json::Value =
            serde_json::from_str(result2.content[0].as_text().unwrap()).unwrap();
        assert!(json2["requests"].as_object().unwrap().is_empty());
        assert!(json2["websocket_frames"].as_object().unwrap().is_empty());
    }

    /// SEC-19: Sensitive headers are redacted by default.
    #[test]
    fn sensitive_headers_are_redacted_by_default() {
        // Given a captured request with Authorization, Cookie and Accept headers
        let request = crate::handler::NetworkRequest {
            url: "https://a/".into(),
            method: "GET".into(),
            resource_type: None,
            request_headers: Some(
                json!({"Authorization": "Bearer t", "cookie": "s=1", "Accept": "*/*"}),
            ),
            request_post_data: None,
            response_status: Some(200),
            response_status_text: None,
            response_headers: Some(json!({"Set-Cookie": "s=2"})),
            response_body: None,
            redirects: Vec::new(),
        };
        let mut requests = std::collections::HashMap::from([("R1".to_string(), request)]);
        // When the default (redacting) path runs
        redact_requests(&mut requests);
        // Then credential headers are hidden and the rest is intact
        let r = &requests["R1"];
        let req = r.request_headers.as_ref().unwrap();
        assert_eq!(req["Authorization"], crate::cli::REDACTED);
        assert_eq!(req["cookie"], crate::cli::REDACTED);
        assert_eq!(req["Accept"], "*/*");
        assert_eq!(
            r.response_headers.as_ref().unwrap()["Set-Cookie"],
            crate::cli::REDACTED
        );
    }
}
