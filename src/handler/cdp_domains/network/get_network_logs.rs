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
    description = "Returns the tab's HTTP requests (one line each: id, method, URL, status, type; the last 3 navigations, oldest first) and WebSocket frames, filtered. 'request_id' gives one request's headers and body; include_details: true gives them for every request (bodies cut at 64000 characters; none over 2 MB). Credential headers and request bodies are redacted unless include_sensitive_headers. 'clear' removes what was returned."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetNetworkLogsTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Remove the returned entries. Default: false.
    #[serde(default)]
    pub clear: Option<bool>,

    /// 'rest', 'websocket' or 'both' (default).
    #[serde(default)]
    pub type_filter: Option<String>,

    /// Case-insensitive URL substring.
    #[serde(default)]
    pub url_filter: Option<String>,

    /// 'sent', 'received' or 'both' (default).
    #[serde(default)]
    pub ws_direction_filter: Option<String>,

    /// Case-insensitive WebSocket payload substring.
    #[serde(default)]
    pub ws_content_filter: Option<String>,

    /// Headers and bodies of every request, as JSON. Default: false (one line per request).
    #[serde(default)]
    pub include_details: Option<bool>,

    /// Show credential headers (Authorization, Cookie, token...) and request bodies. Default: false.
    #[serde(default)]
    pub include_sensitive_headers: Option<bool>,

    /// Only the current navigation. Default: false.
    #[serde(default)]
    pub current_navigation_only: Option<bool>,

    /// One request's headers and body (an id from the summary).
    #[serde(default)]
    pub request_id: Option<String>,
}

type Requests = HashMap<String, NetworkRequest>;
type Frames = HashMap<String, VecDeque<WebSocketFrame>>;

/// The note of the call that opened the connection: Chrome replays console
/// messages to a new connection, but not network traffic.
const NOT_CAPTURED_BEFORE: &str = "this call connected to the browser: requests sent before it are not captured (reload to see the page's own requests)";

/// The parsed filters of one call, all lowercase.
struct Filters {
    want_rest: bool,
    want_ws: bool,
    url: String,
    ws_direction: String,
    ws_content: String,
    include_details: bool,
    request_id: Option<String>,
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
            include_details: args.include_details.unwrap_or(false),
            request_id: args.request_id.clone(),
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
    /// Ids of `requests`, oldest first.
    pub(crate) order: Vec<String>,
}

impl GetNetworkLogsTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: GetNetworkLogsTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        // Connected first (see get_console_logs): an instance nothing has
        // connected to yet has empty buffers.
        let first = !session.is_connected().await;
        let target = session.target(args.tab_id.clone()).await?;
        if first {
            session.settle_replayed_events().await;
        }
        let network_state = session.network_state(args.tab_id.clone())?;
        let mut snapshot = args.collect(&network_state).await;
        let mut notes: Vec<String> = first
            .then(|| NOT_CAPTURED_BEFORE.to_string())
            .into_iter()
            .collect();
        if args.wants_bodies() {
            let loading =
                fetch_bodies(&target, &mut snapshot.requests, &snapshot.current_ids).await;
            notes.extend(still_loading_note(loading));
        }
        args.finish(snapshot, notes)
    }

    /// Whether response bodies should be fetched (REST requests with
    /// details, or the one `request_id` names).
    pub(crate) fn wants_bodies(&self) -> bool {
        let filters = Filters::new(self);
        filters.want_rest && (filters.include_details || filters.request_id.is_some())
    }

    /// Copies the tab's buffers and applies the type, URL, direction and
    /// content filters; with `clear`, removes from the buffers exactly what
    /// is returned (connections keep their URL). Shared by every browser.
    pub(crate) async fn collect(
        &self,
        network_state: &std::sync::Arc<tokio::sync::Mutex<crate::handler::NetworkState>>,
    ) -> NetworkSnapshot {
        let filters = Filters::new(self);
        let mut st = network_state.lock().await;
        let current_only = self.current_navigation_only.unwrap_or(false);
        let requests = if current_only {
            st.iter_current()
        } else {
            st.iter_all()
        };
        let mut snapshot = NetworkSnapshot {
            requests,
            frames: st.ws_frames.clone().into_iter().collect(),
            current_ids: st.current_ids(),
            order: st.ordered_ids(current_only),
        };
        filters.apply(&mut snapshot);
        if self.clear.unwrap_or(false) {
            st.remove_requests(snapshot.requests.keys());
            for id in snapshot.frames.keys() {
                if let Some(frames) = st.ws_frames.get_mut(id) {
                    frames.retain(|f| !filters.frame_matches(f));
                }
            }
        }
        snapshot
    }
}

impl Filters {
    /// Keeps the requests and frames the filters select.
    fn apply(&self, snapshot: &mut NetworkSnapshot) {
        let filters = self;
        if filters.want_rest {
            let one = filters.request_id.as_deref();
            snapshot
                .requests
                .retain(|id, req| filters.url_matches(&req.url) && one.is_none_or(|one| one == id));
        } else {
            snapshot.requests.clear();
        }
        let kept = &snapshot.requests;
        snapshot.order.retain(|id| kept.contains_key(id));
        if filters.want_ws && filters.request_id.is_none() {
            snapshot.frames.retain(|_, frames| {
                frames.retain(|f| filters.frame_matches(f));
                !frames.is_empty()
            });
        } else {
            snapshot.frames.clear();
        }
    }
}

impl GetNetworkLogsTool {
    /// The tool's output: one line per request and frame by default, the
    /// one request `request_id` names, or every detail as JSON; redacted
    /// unless asked otherwise; `notes` (browser limits, lost events) last.
    pub(crate) fn finish(
        &self,
        mut snapshot: NetworkSnapshot,
        notes: Vec<String>,
    ) -> Result<CallToolResult, CallToolError> {
        let filters = Filters::new(self);
        let detailed = filters.include_details || filters.request_id.is_some();
        if detailed && !self.include_sensitive_headers.unwrap_or(false) {
            redact_requests(&mut snapshot.requests);
        }
        if let Some(id) = &filters.request_id {
            return one_request(id, snapshot.requests, notes);
        }
        if !filters.include_details {
            let text = summary(&filters, &snapshot, &notes);
            return Ok(CallToolResult::text_content(vec![text.into()]));
        }
        let mut output = serde_json::Map::new();
        if filters.want_rest {
            let requests = requests_json(snapshot.requests);
            output.insert("requests".into(), requests);
        }
        if filters.want_ws {
            let frames = serde_json::to_value(&snapshot.frames).unwrap_or_default();
            output.insert("websocket_frames".into(), frames);
        }
        if !notes.is_empty() {
            output.insert("notes".into(), json!(notes));
        }
        Ok(CallToolResult::text_content(vec![
            serde_json::to_string_pretty(&output)
                .unwrap_or_default()
                .into(),
        ]))
    }
}

/// Longest URL in a summary line (characters).
const URL_MAX_CHARS: usize = 200;

/// Longest WebSocket payload preview in a summary line (characters).
const PAYLOAD_PREVIEW_CHARS: usize = 100;

/// `text` on one line, cut at `max` characters with an ellipsis.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// The default reply: a line per request, oldest first, then a line per
/// WebSocket frame by connection, then the notes.
fn summary(filters: &Filters, snapshot: &NetworkSnapshot, notes: &[String]) -> String {
    let mut lines = Vec::new();
    if filters.want_rest {
        lines.push(format!(
            "{} request(s), oldest first (request_id shows one's headers and body):",
            snapshot.order.len()
        ));
        let requests = &snapshot.order;
        let rows = requests
            .iter()
            .filter_map(|id| Some(summary_line(id, snapshot.requests.get(id)?)));
        lines.extend(rows);
    }
    let count: usize = snapshot.frames.values().map(VecDeque::len).sum();
    // No frames: said only when frames were asked for alone.
    if filters.want_ws && (count > 0 || !filters.want_rest) {
        lines.push(format!("{count} WebSocket frame(s), by connection:"));
        let mut connections: Vec<&String> = snapshot.frames.keys().collect();
        connections.sort();
        for id in connections {
            lines.extend(
                snapshot.frames[id]
                    .iter()
                    .map(|frame| frame_line(id, frame)),
            );
        }
    }
    lines.extend(notes.iter().map(|note| format!("Note: {note}")));
    lines.join("\n")
}

/// One summary line: `id METHOD url => status text [type]`.
fn summary_line(id: &str, request: &NetworkRequest) -> String {
    let outcome = match (&request.error, request.response_status) {
        (Some(error), _) => format!("failed: {error}"),
        (None, Some(status)) => match request.response_status_text.as_deref() {
            Some(text) if !text.is_empty() => format!("{status} {text}"),
            _ => status.to_string(),
        },
        (None, None) => "pending".to_string(),
    };
    let kind = request
        .resource_type
        .as_deref()
        .map(|kind| format!(" [{kind}]"))
        .unwrap_or_default();
    let url = one_line(&request.url, URL_MAX_CHARS);
    format!("{id} {} {url} => {outcome}{kind}", request.method)
}

/// One WebSocket frame: `id sent|received url (bytes): payload preview`.
fn frame_line(id: &str, frame: &WebSocketFrame) -> String {
    let direction = if frame.is_sent { "sent" } else { "received" };
    let payload = one_line(&frame.payload_data, PAYLOAD_PREVIEW_CHARS);
    format!(
        "{id} {direction} {} ({} bytes): {payload}",
        frame.url, frame.payload_len
    )
}

/// The request `id` with its headers and body, or an error when the tab
/// did not log it (or the filters left it out).
fn one_request(
    id: &str,
    mut requests: Requests,
    notes: Vec<String>,
) -> Result<CallToolResult, CallToolError> {
    let Some(request) = requests.remove(id) else {
        return Err(CallToolError::from_message(format!(
            "no request '{id}' in the tab's log: take an id from the get_network_logs summary (the last 3 navigations are kept)"
        )));
    };
    let detailed = requests_json(HashMap::from([(id.to_string(), request)]));
    let mut output = serde_json::Map::new();
    output.insert("request_id".into(), json!(id));
    if let Some(fields) = detailed.get(id).and_then(serde_json::Value::as_object) {
        output.extend(fields.clone());
    }
    if !notes.is_empty() {
        output.insert("notes".into(), json!(notes));
    }
    let text = serde_json::to_string_pretty(&output).unwrap_or_default();
    Ok(CallToolResult::text_content(vec![text.into()]))
}

/// Largest response body fetched for `get_network_logs` (bytes): Firefox
/// keeps none larger, and on Chrome a huge reply would drop the connection.
pub(crate) const BODY_LIMIT: u64 = 2_000_000;

/// Most characters of a request or response body in a reply.
const BODY_MAX_CHARS: usize = 64_000;

/// Longest wait for one response body: a body is only asked for once its
/// response finished, so this only bounds a browser that does not answer.
pub(crate) const BODY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the body of `request` (`id`) is asked for: an answered request
/// of the current navigation, finished loading, without a body yet, and
/// not over [`BODY_LIMIT`].
pub(crate) fn wants_body(id: &str, request: &NetworkRequest, current: &HashSet<String>) -> bool {
    current.contains(id)
        && request.response_status.is_some()
        && request.finished
        && request.response_body.is_none()
        && content_length(request).is_none_or(|length| length <= BODY_LIMIT)
}

/// How many answered requests of the current navigation are still loading
/// (an open stream such as server-sent events): their bodies are not asked
/// for, as the browser would only answer once they end.
pub(crate) fn still_loading(requests: &Requests, current: &HashSet<String>) -> usize {
    requests
        .iter()
        .filter(|(id, r)| current.contains(*id) && r.response_status.is_some() && !r.finished)
        .count()
}

/// The note for `count` responses still loading, when there are any.
pub(crate) fn still_loading_note(count: usize) -> Option<String> {
    let plural = if count == 1 { "response" } else { "responses" };
    (count > 0).then(|| {
        format!("{count} {plural} still loading (e.g. an open event stream): no body until it ends")
    })
}

/// `Network.getResponseBody` for the requests [`wants_body`] picks (bodies
/// are only kept in the returned copy); how many are still loading.
async fn fetch_bodies(
    target: &CdpTarget,
    requests: &mut Requests,
    current: &HashSet<String>,
) -> usize {
    let loading = still_loading(requests, current);
    for (req_id, req) in requests.iter_mut() {
        if !wants_body(req_id, req, current) {
            continue;
        }
        let ask = target.send_raw_command("Network.getResponseBody", json!({"requestId": req_id}));
        let reply = tokio::time::timeout(BODY_WAIT, ask).await;
        if let Ok(Ok(reply)) = reply
            && let Some(result) = reply.result.as_ref()
        {
            req.response_body = result
                .get("body")
                .and_then(|b| b.as_str())
                .map(str::to_string);
            let base64 = result.get("base64Encoded").and_then(|b| b.as_bool()) == Some(true);
            req.response_body_encoding = base64.then(|| "base64".to_string());
        }
    }
    loading
}

/// The response's `Content-Length` header, when it has one.
fn content_length(request: &NetworkRequest) -> Option<u64> {
    request
        .response_header("content-length")?
        .trim()
        .parse()
        .ok()
}

/// The requests with their details, bodies cut at [`BODY_MAX_CHARS`].
fn requests_json(mut requests: Requests) -> serde_json::Value {
    let cap = |body: &mut Option<String>| {
        if let Some(text) = body {
            *text = crate::util::text::capped(text, BODY_MAX_CHARS);
        }
    };
    for request in requests.values_mut() {
        cap(&mut request.response_body);
        cap(&mut request.request_post_data);
    }
    serde_json::to_value(&requests).unwrap_or_default()
}

/// Hides the values of credential-carrying headers and the request bodies
/// (login forms, OAuth token exchanges) in every request.
fn redact_requests(requests: &mut Requests) {
    for request in requests.values_mut() {
        if request.request_post_data.is_some() {
            request.request_post_data = Some(crate::cli::REDACTED.to_string());
        }
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
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;
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
                response_status: Some(200),
                response_status_text: Some("OK".to_string()),
                ..Default::default()
            },
        );

        st.insert_request(
            "req-2".to_string(),
            NetworkRequest {
                url: "https://google.com/search".to_string(),
                method: "GET".to_string(),
                resource_type: Some("Document".to_string()),
                response_status: Some(200),
                response_status_text: Some("OK".to_string()),
                ..Default::default()
            },
        );

        // Mock WS frames
        st.push_ws_frame(
            "ws-1".to_string(),
            WebSocketFrame::new(
                "wss://socket.com/feed".to_string(),
                "hello server".to_string(),
                true,
            ),
        );
        st.push_ws_frame(
            "ws-1".to_string(),
            WebSocketFrame::new(
                "wss://socket.com/feed".to_string(),
                "welcome client".to_string(),
                false,
            ),
        );
    }

    fn answered(headers: serde_json::Value, body: Option<String>) -> NetworkRequest {
        NetworkRequest {
            url: "https://example.com/big".to_string(),
            method: "POST".to_string(),
            resource_type: Some("XHR".to_string()),
            request_post_data: Some("p".repeat(BODY_MAX_CHARS + 1)),
            response_status: Some(200),
            response_status_text: Some("OK".to_string()),
            response_headers: Some(headers),
            response_body: body,
            finished: true,
            ..Default::default()
        }
    }

    /// AUD-OB-14: a response still loading (an open stream) is not asked
    /// for its body (Firefox waited 10 s for one), and is counted for a
    /// note.
    #[tokio::test]
    async fn unfinished_responses_are_noted_not_fetched() {
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let target = handler
            .session(None)
            .await
            .unwrap()
            .target(None)
            .await
            .unwrap();
        let streaming = NetworkRequest {
            finished: false,
            ..answered(json!({}), None)
        };
        let mut requests = HashMap::from([
            ("done".to_string(), answered(json!({}), None)),
            ("sse".to_string(), streaming),
        ]);
        let current = HashSet::from(["done".to_string(), "sse".to_string()]);
        let loading = fetch_bodies(&target, &mut requests, &current).await;
        assert_eq!(loading, 1);
        let fetched = mock.commands_named("Network.getResponseBody");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].params["requestId"], "done");
        let note = still_loading_note(1).unwrap();
        assert!(note.starts_with("1 response still loading"), "{note}");
        assert_eq!(still_loading_note(0), None);
    }

    /// AUD-OB-13: bodies are cut in the reply, on every browser.
    #[test]
    fn large_bodies_are_capped_in_the_reply() {
        let args: GetNetworkLogsTool = serde_json::from_value(
            json!({"include_sensitive_headers": true, "include_details": true}),
        )
        .unwrap();
        let body = Some("b".repeat(1_000_000));
        let snapshot = NetworkSnapshot {
            requests: HashMap::from([("r1".to_string(), answered(json!({}), body))]),
            frames: HashMap::new(),
            current_ids: HashSet::new(),
            order: vec!["r1".to_string()],
        };
        let reply = args.finish(snapshot, Vec::new()).unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        let output: serde_json::Value = serde_json::from_str(&text).unwrap();
        let request = &output["requests"]["r1"];
        let cut = |key: &str| request[key].as_str().unwrap().to_string();
        assert!(cut("response_body").ends_with("[cut: the first 64000 of 1000000 characters]"));
        assert!(cut("request_post_data").ends_with("[cut: the first 64000 of 64001 characters]"));
    }

    /// AUD-OB-15: a binary body says it is base64 (it read as text).
    #[tokio::test]
    async fn base64_bodies_are_marked() {
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Network.getResponseBody",
            |_| json!({"result": {"body": "iVBORw0K", "base64Encoded": true}}),
        );
        let handler = mock_handler(mock.http_port);
        let target = handler
            .session(None)
            .await
            .unwrap()
            .target(None)
            .await
            .unwrap();
        let mut requests = HashMap::from([("img".to_string(), answered(json!({}), None))]);
        let current = HashSet::from(["img".to_string()]);
        fetch_bodies(&target, &mut requests, &current).await;
        let request = &requests["img"];
        assert_eq!(request.response_body.as_deref(), Some("iVBORw0K"));
        assert_eq!(request.response_body_encoding.as_deref(), Some("base64"));
        let row = serde_json::to_value(request).unwrap();
        assert_eq!(row["response_body_encoding"], "base64");
    }

    /// ISSUE-18: by default the reply is one line per request, oldest
    /// first, without headers or bodies, then one line per WebSocket frame;
    /// without frames, no WebSocket line unless only frames were asked for.
    #[tokio::test]
    async fn default_reply_is_one_line_per_request() {
        // Given a tab that logged two requests and two frames
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;
        let read = |args: serde_json::Value| {
            let params = crate::handler::cdp_domains::tests::call("get_network_logs", args);
            GetNetworkLogsTool::handle(params, &handler)
        };
        // When get_network_logs runs without options
        let reply = read(json!({})).await.unwrap();
        // Then each request and frame is one line, in order
        let text = reply.content[0].as_text().unwrap().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("2 request(s), oldest first"), "{text}");
        assert_eq!(
            lines[1],
            "req-1 GET https://example.com/api/v1 => 200 OK [XHR]"
        );
        assert_eq!(
            lines[2],
            "req-2 GET https://google.com/search => 200 OK [Document]"
        );
        assert_eq!(lines[3], "2 WebSocket frame(s), by connection:");
        assert_eq!(
            lines[4],
            "ws-1 sent wss://socket.com/feed (12 bytes): hello server"
        );
        assert_eq!(
            lines[5],
            "ws-1 received wss://socket.com/feed (14 bytes): welcome client"
        );
        assert!(!text.contains("request_headers"), "{text}");
        // And without frames there is no WebSocket line, unless only frames were asked for
        handler
            .default_session
            .network_state
            .lock()
            .await
            .ws_frames
            .clear();
        let reply = read(json!({})).await.unwrap();
        assert!(!reply.content[0].as_text().unwrap().contains("WebSocket"));
        let reply = read(json!({"type_filter": "websocket"})).await.unwrap();
        assert_eq!(
            reply.content[0].as_text().unwrap(),
            "0 WebSocket frame(s), by connection:"
        );
    }

    /// ISSUE-18: a failed or unanswered request says so; a long URL is cut.
    #[test]
    fn summary_lines_show_failures() {
        let failed = NetworkRequest {
            url: "http://nope.invalid/".into(),
            method: "GET".into(),
            error: Some("net::ERR_NAME_NOT_RESOLVED".into()),
            ..Default::default()
        };
        assert_eq!(
            summary_line("r1", &failed),
            "r1 GET http://nope.invalid/ => failed: net::ERR_NAME_NOT_RESOLVED"
        );
        let pending = NetworkRequest {
            url: "http://a/x".into(),
            method: "POST".into(),
            ..Default::default()
        };
        assert_eq!(
            summary_line("r2", &pending),
            "r2 POST http://a/x => pending"
        );
        let long = NetworkRequest {
            url: format!("http://a/{}", "q".repeat(500)),
            method: "GET".into(),
            response_status: Some(204),
            ..Default::default()
        };
        let line = summary_line("r3", &long);
        assert!(line.len() < 260 && line.contains("… => 204"), "{line}");
    }

    /// ISSUE-18: request_id expands one request (headers, body); an id the
    /// tab did not log fails saying where ids come from.
    #[tokio::test]
    async fn request_id_expands_one_request() {
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;
        let read = |args: serde_json::Value| {
            let params = crate::handler::cdp_domains::tests::call("get_network_logs", args);
            GetNetworkLogsTool::handle(params, &handler)
        };
        let reply = read(json!({"request_id": "req-1"})).await.unwrap();
        let one: serde_json::Value =
            serde_json::from_str(reply.content[0].as_text().unwrap()).unwrap();
        assert_eq!(one["request_id"], "req-1");
        assert_eq!(one["url"], "https://example.com/api/v1");
        assert!(one.get("request_headers").is_some(), "{one}");
        let error = read(json!({"request_id": "nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("'nope'") && error.contains("summary"),
            "{error}"
        );
    }

    /// AUD-OB-13: Chrome does not fetch a body over BODY_LIMIT (Firefox
    /// does not keep one): bodies that large are slow, and huge ones would
    /// drop the connection.
    #[tokio::test]
    async fn bodies_over_the_limit_are_not_fetched() {
        use crate::handler::cdp_domains::tests::{mock_handler, spawn_recording_mock};
        let mock = spawn_recording_mock().await;
        let handler = mock_handler(mock.http_port);
        let target = handler
            .session(None)
            .await
            .unwrap()
            .target(None)
            .await
            .unwrap();
        let mut requests = HashMap::from([
            (
                "big".to_string(),
                answered(json!({"content-length": "20000000"}), None),
            ),
            (
                "small".to_string(),
                answered(json!({"Content-Length": "12"}), None),
            ),
        ]);
        let current = HashSet::from(["big".to_string(), "small".to_string()]);
        fetch_bodies(&target, &mut requests, &current).await;
        let fetched = mock.commands_named("Network.getResponseBody");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].params["requestId"], "small");
    }

    #[tokio::test]
    async fn test_get_network_logs_filter_type_rest() {
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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
    async fn test_get_network_logs_clear() {
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;

        let params: CallToolRequestParams = serde_json::from_value(json!({
            "name": "get_network_logs",
            "arguments": {
                "include_details": true,
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

    /// AUD-OB-04 / AUD-OB-05: `clear` removes only what the call returned
    /// (it used to empty every request and frame whatever the filters), and
    /// a live connection keeps its URL.
    #[tokio::test]
    async fn clear_removes_only_the_returned_entries() {
        let handler = McpHandler::new_test_with_port(spawn_mock_chrome_server().await);
        setup_mock_data(&handler).await;
        handler
            .default_session
            .network_state
            .lock()
            .await
            .insert_ws_connection("ws-1".into(), "wss://socket.com/feed".into());
        let read = |args: serde_json::Value| {
            let handler = &handler;
            async move {
                let params = crate::handler::cdp_domains::tests::call("get_network_logs", args);
                let reply = GetNetworkLogsTool::handle(params, handler).await.unwrap();
                serde_json::from_str::<serde_json::Value>(reply.content[0].as_text().unwrap())
                    .unwrap()
            }
        };
        // When the example.com request is read with clear
        let cleared =
            read(json!({"url_filter": "example.com", "type_filter": "rest", "clear": true, "include_details": true})).await;
        assert_eq!(cleared["requests"].as_object().unwrap().len(), 1);
        // Then the other request and every frame are still there
        let rest = read(json!({"include_details": true})).await;
        let ids: Vec<&String> = rest["requests"].as_object().unwrap().keys().collect();
        assert_eq!(ids, ["req-2"]);
        assert!(
            !rest["websocket_frames"]["ws-1"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // And clearing the frames keeps the connection's URL for later ones
        read(json!({"type_filter": "websocket", "clear": true, "include_details": true})).await;
        let mut st = handler.default_session.network_state.lock().await;
        assert!(st.ws_frames.get_mut("ws-1").is_none_or(|f| f.is_empty()));
        assert_eq!(
            st.ws_connections.get("ws-1").map(String::as_str),
            Some("wss://socket.com/feed")
        );
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
            ..Default::default()
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

    /// AUD-OB-03: request bodies and API-key style headers are redacted by
    /// default too (a login POST body used to reach the agent in full).
    #[test]
    fn request_bodies_and_token_headers_are_redacted_by_default() {
        // Given a login POST with API-key and CSRF headers
        let headers = json!({
            "X-Api-Key": "k", "X-CSRF-Token": "c", "x-goog-api-key": "g",
            "X-Session-Id": "s", "WWW-Authenticate": "Basic", "Content-Type": "text/plain"
        });
        let request = crate::handler::NetworkRequest {
            url: "https://a/login".into(),
            method: "POST".into(),
            request_headers: Some(headers),
            request_post_data: Some("user=alice&password=AUDIT-SECRET-PW".into()),
            response_status: Some(200),
            ..Default::default()
        };
        let mut requests = std::collections::HashMap::from([("R1".to_string(), request)]);
        // When the default (redacting) path runs
        redact_requests(&mut requests);
        // Then the body and every credential-like header are hidden
        let r = &requests["R1"];
        assert_eq!(r.request_post_data.as_deref(), Some(crate::cli::REDACTED));
        let req = r.request_headers.as_ref().unwrap();
        for name in [
            "X-Api-Key",
            "X-CSRF-Token",
            "x-goog-api-key",
            "X-Session-Id",
        ] {
            assert_eq!(req[name], crate::cli::REDACTED, "{name}");
        }
        // And challenges and ordinary headers stay readable
        assert_eq!(req["WWW-Authenticate"], "Basic");
        assert_eq!(req["Content-Type"], "text/plain");
    }
}
