pub mod cancel_invocation;
pub mod get_invocation;
pub mod invoke_tool;
pub mod list_invocations;
pub mod list_tools;

pub use cancel_invocation::CancelWebmcpInvocationTool;
pub use get_invocation::GetWebmcpInvocationTool;
pub use invoke_tool::InvokeWebmcpToolTool;
pub use list_invocations::ListWebmcpInvocationsTool;
pub use list_tools::ListWebmcpToolsTool;

use cdp_browser_lite::WsResponse;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Everything the `Page` listener needs to keep the per-tab WebMCP tool cache
/// aligned across navigations: the owning tab's `WebmcpState` and a live
/// handle to that same tab's CDP target for the enable-based resync.
///
/// `None` wiring means "no WebMCP for this session or tab": the page listener
/// then skips every purge/resync step for its events.
#[derive(Clone)]
pub(crate) struct WebmcpNavSync {
    pub(crate) state: Arc<Mutex<WebmcpState>>,
    pub(crate) target: crate::handler::cdp_domains::cdp_target::CdpTarget,
}

/// The annotations a page declared on a WebMCP tool, as Chrome reports
/// them (from `registerTool`'s `readOnlyHint`, `untrustedContentHint`,
/// `consequentialHint` and `debugging`; `autosubmit` for declarative
/// tools). A field a later Chrome adds is kept in `other`: an unknown field
/// used to be dropped, which hid `consequential` from the agent.
#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct WebmcpAnnotation {
    #[serde(rename = "readOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// The tool's output may hold user-generated or third-party content.
    #[serde(rename = "untrustedContent", skip_serializing_if = "Option::is_none")]
    pub untrusted_content: Option<bool>,
    /// Running the tool has real-world effects (a booking, a payment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consequential: Option<bool>,
    /// The tool is meant for developer tooling, not end users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debugging: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autosubmit: Option<bool>,
    /// Annotations this server does not know yet, passed through as they are.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, ::serde::Serialize, ::serde::Deserialize)]
pub struct WebmcpTool {
    pub name: String,
    pub description: String,
    /// Optional in the protocol: a tool without parameters may omit it.
    #[serde(
        rename = "inputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<WebmcpAnnotation>,
    #[serde(rename = "frameId")]
    pub frame_id: String,
    #[serde(rename = "backendNodeId", skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
}

#[derive(Clone, Debug, Default, ::serde::Serialize, ::serde::Deserialize)]
pub struct WebmcpInvocation {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "frameId")]
    pub frame_id: String,
    #[serde(rename = "invocationId")]
    pub invocation_id: String,
    pub input: String,
    pub status: Option<String>, // "Completed", "Canceled", "Error"
    pub output: Option<serde_json::Value>,
    #[serde(rename = "errorText", skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    /// The invoked tool declares untrusted output (`untrustedContent`):
    /// replies with its output say to treat it as data.
    #[serde(skip)]
    pub untrusted_content: bool,
    /// Why a pending invocation may never complete (its page navigated
    /// away); cleared when its answer still comes (ISSUE-73).
    #[serde(skip)]
    pub note: Option<String>,
}

/// The note of a pending invocation whose page navigated away.
pub(crate) const PAGE_GONE: &str =
    "its page navigated away, so it may never complete: cancel it with webmcp_cancel_invocation";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize, Default)]
pub enum WebmcpAvailability {
    #[default]
    NotRequested,
    Pending,
    Unsupported,
    Enabled,
}

impl WebmcpAvailability {
    /// Returns initial availability based on whether WebMCP is enabled on the server/session.
    pub fn initial(enable_webmcp: bool) -> Self {
        if enable_webmcp {
            Self::Pending
        } else {
            Self::NotRequested
        }
    }
}

/// Most WebMCP invocations remembered per tab.
pub(crate) const MAX_INVOCATIONS: usize = 1000;

#[derive(Default, Debug, Clone)]
pub struct WebmcpState {
    // Maps frame_id -> (tool_name -> Tool)
    pub tools: HashMap<String, HashMap<String, WebmcpTool>>,
    // Maps invocation_id -> Invocation (oldest forgotten beyond the cap)
    pub invocations: crate::util::bounded::BoundedMap<String, WebmcpInvocation, MAX_INVOCATIONS>,
    // Tracks current availability of the WebMCP feature
    pub availability: WebmcpAvailability,
}

impl WebmcpState {
    /// Drops every cached tool registered by `frame_id`.
    ///
    /// Other frames' tools, invocations and availability are untouched.
    /// Invocations must survive document swaps: a page tool whose `execute`
    /// reloads the page still emits `toolResponded` afterwards, and
    /// `webmcp_get_invocation` must keep working.
    pub(crate) fn clear_frame_tools(&mut self, frame_id: &str) {
        self.tools.remove(frame_id);
        self.note_gone(|invocation| invocation.frame_id == frame_id);
    }

    /// Notes the pending invocations `of` selects that their page is gone
    /// (they stayed `Pending` forever, ISSUE-73).
    fn note_gone(&mut self, of: impl Fn(&WebmcpInvocation) -> bool) {
        for invocation in self.invocations.values_mut() {
            if invocation.status.is_none() && of(invocation) {
                invocation.note = Some(PAGE_GONE.to_string());
            }
        }
    }

    /// The frame registering the tool `name`, for a call without
    /// `frameId`: an error names the frames when several have it, or how
    /// to list the tools when none has.
    pub(crate) fn frame_of(&self, name: &str) -> Result<String, String> {
        let mut frames: Vec<&String> = self
            .tools
            .iter()
            .filter(|(_, tools)| tools.contains_key(name))
            .map(|(frame, _)| frame)
            .collect();
        frames.sort();
        match frames.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!(
                "No frame registered the WebMCP tool '{name}': call webmcp_list_tools to see the page's tools."
            )),
            many => Err(format!(
                "Several frames register the WebMCP tool '{name}' ({}): pass frameId.",
                many.iter()
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Drops every cached tool across all frames.
    ///
    /// A cross-document navigation of the main frame destroys the whole frame
    /// tree; child frames get fresh frame IDs, so a single-frame purge would
    /// leave stale iframe tools behind. Invocations survive for the same
    /// reason as in [`Self::clear_frame_tools`].
    pub(crate) fn clear_all_tools(&mut self) {
        self.tools.clear();
        self.note_gone(|_| true);
    }
}

pub(crate) async fn process_webmcp_event(event: &WsResponse, state: &Arc<Mutex<WebmcpState>>) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    let tools = params.get("tools").and_then(|v| v.as_array());
    let mut st = state.lock().await;
    match (method, tools) {
        ("WebMCP.toolsAdded", Some(tools)) => st.add_tools(tools),
        ("WebMCP.toolsRemoved", Some(tools)) => st.remove_tools(tools),
        ("WebMCP.toolInvoked", _) => {
            if let Ok(mut invocation) = serde_json::from_value::<WebmcpInvocation>(params.clone()) {
                invocation.untrusted_content =
                    st.declares_untrusted(&invocation.frame_id, &invocation.tool_name);
                st.invocations
                    .insert(invocation.invocation_id.clone(), invocation);
            }
        }
        ("WebMCP.toolResponded", _) => st.record_response(params),
        _ => {}
    }
}

impl WebmcpState {
    fn add_tools(&mut self, tools: &[serde_json::Value]) {
        for value in tools {
            if let Ok(tool) = serde_json::from_value::<WebmcpTool>(value.clone()) {
                self.tools
                    .entry(tool.frame_id.clone())
                    .or_default()
                    .insert(tool.name.clone(), tool);
            }
        }
    }

    /// Whether the tool `name` of `frame_id` declares untrusted output.
    fn declares_untrusted(&self, frame_id: &str, name: &str) -> bool {
        self.tools
            .get(frame_id)
            .and_then(|tools| tools.get(name))
            .and_then(|tool| tool.annotations.as_ref())
            .and_then(|annotations| annotations.untrusted_content)
            == Some(true)
    }

    fn remove_tools(&mut self, tools: &[serde_json::Value]) {
        for value in tools {
            let name = value.get("name").and_then(|v| v.as_str());
            let frame_id = value.get("frameId").and_then(|v| v.as_str());
            if let (Some(name), Some(frame_id)) = (name, frame_id)
                && let Some(frame_tools) = self.tools.get_mut(frame_id)
            {
                frame_tools.remove(name);
            }
        }
    }

    fn record_response(&mut self, params: &serde_json::Value) {
        let Some(invocation_id) = params.get("invocationId").and_then(|v| v.as_str()) else {
            return;
        };
        if let Some(inv) = self.invocations.get_mut(invocation_id) {
            inv.status = params
                .get("status")
                .and_then(|v| v.as_str())
                .map(String::from);
            inv.output = params.get("output").cloned();
            inv.error_text = params
                .get("errorText")
                .and_then(|v| v.as_str())
                .map(String::from);
            inv.note = None;
        }
    }
}

pub(crate) fn start_webmcp_listener(
    target: &crate::handler::cdp_domains::cdp_target::CdpTarget,
    state_clone: Arc<Mutex<WebmcpState>>,
) -> tokio::task::JoinHandle<()> {
    let nav_sync = WebmcpNavSync {
        state: state_clone.clone(),
        target: target.clone(),
    };
    crate::handler::cdp_domains::event_pump::spawn_domain_listener_with_recovery(
        target,
        "WebMCP",
        {
            let state = state_clone.clone();
            move |event| {
                let state = state.clone();
                async move {
                    process_webmcp_event(&event, &state).await;
                }
            }
        },
        // A lag on this stream is what the tool cache cannot survive on its
        // own: `toolsAdded` and `toolsRemoved` may both have been dropped.
        move |_skipped| {
            let nav_sync = nav_sync.clone();
            async move {
                nav_sync.resync_after_lag().await;
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(method: &str, params: serde_json::Value) -> WsResponse {
        WsResponse {
            method: Some(method.to_string()),
            params: Some(params),
            ..Default::default()
        }
    }

    /// AUD-RW-16: a tool registered without `inputSchema` (optional in the
    /// protocol) is kept, and listed without one (it used to be dropped).
    #[tokio::test]
    async fn tools_without_input_schema_are_kept() {
        let state = Arc::new(Mutex::new(WebmcpState::default()));
        let add_event = make_event(
            "WebMCP.toolsAdded",
            json!({"tools": [{"name": "no-schema", "description": "d", "frameId": "f1"}]}),
        );
        process_webmcp_event(&add_event, &state).await;
        let st = state.lock().await;
        let tool = &st.tools["f1"]["no-schema"];
        let listed = serde_json::to_value(tool).unwrap();
        assert_eq!(
            listed,
            json!({"name": "no-schema", "description": "d", "frameId": "f1"})
        );
    }

    #[tokio::test]
    async fn test_tools_added_and_removed() {
        let state = Arc::new(Mutex::new(WebmcpState::default()));

        let add_event = make_event(
            "WebMCP.toolsAdded",
            json!({
                "tools": [
                    {
                        "name": "testTool",
                        "description": "A test tool",
                        "inputSchema": { "type": "object" },
                        "frameId": "frame-1"
                    }
                ]
            }),
        );
        process_webmcp_event(&add_event, &state).await;

        {
            let st = state.lock().await;
            assert_eq!(
                st.tools
                    .get("frame-1")
                    .unwrap()
                    .get("testTool")
                    .unwrap()
                    .name,
                "testTool"
            );
        }

        let remove_event = make_event(
            "WebMCP.toolsRemoved",
            json!({
                "tools": [
                    {
                        "name": "testTool",
                        "frameId": "frame-1"
                    }
                ]
            }),
        );
        process_webmcp_event(&remove_event, &state).await;

        {
            let st = state.lock().await;
            assert!(st.tools.get("frame-1").unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn test_tool_invoked_and_responded() {
        let state = Arc::new(Mutex::new(WebmcpState::default()));

        let invoke_event = make_event(
            "WebMCP.toolInvoked",
            json!({
                "toolName": "testTool",
                "frameId": "frame-1",
                "invocationId": "inv-1",
                "input": "{\"foo\":\"bar\"}"
            }),
        );
        process_webmcp_event(&invoke_event, &state).await;

        {
            let st = state.lock().await;
            assert_eq!(st.invocations.get("inv-1").unwrap().tool_name, "testTool");
            assert!(st.invocations.get("inv-1").unwrap().status.is_none());
        }

        let respond_event = make_event(
            "WebMCP.toolResponded",
            json!({
                "invocationId": "inv-1",
                "status": "Completed",
                "output": { "result": 42 }
            }),
        );
        process_webmcp_event(&respond_event, &state).await;

        {
            let st = state.lock().await;
            let inv = st.invocations.get("inv-1").unwrap();
            assert_eq!(inv.status.as_deref(), Some("Completed"));
            assert_eq!(
                inv.output.as_ref().unwrap().get("result").unwrap().as_i64(),
                Some(42)
            );
        }
    }

    fn seed_tools(st: &mut WebmcpState) {
        for (name, frame) in [
            ("toolA", "frame-main"),
            ("toolI1", "frame-iframe"),
            ("toolI2", "frame-iframe"),
        ] {
            st.tools.entry(frame.to_string()).or_default().insert(
                name.to_string(),
                WebmcpTool {
                    name: name.to_string(),
                    description: "d".into(),
                    input_schema: Some(json!({})),
                    annotations: None,
                    frame_id: frame.to_string(),
                    backend_node_id: None,
                },
            );
        }
    }

    /// A pending invocation of `frame`.
    fn pending(id: &str, frame: &str) -> WebmcpInvocation {
        WebmcpInvocation {
            tool_name: "toolA".into(),
            frame_id: frame.into(),
            invocation_id: id.into(),
            ..Default::default()
        }
    }

    /// ISSUE-73: a pending invocation whose page navigated away is noted
    /// (it may never complete); its answer, if it still comes, replaces the
    /// note. Finished invocations and other frames' are untouched.
    #[test]
    fn pending_invocations_of_a_gone_page_are_noted() {
        // Given pending invocations in two frames and a finished one
        let mut st = WebmcpState::default();
        seed_tools(&mut st);
        for invocation in [
            pending("p-main", "frame-main"),
            pending("p-frame", "frame-iframe"),
        ] {
            st.invocations
                .insert(invocation.invocation_id.clone(), invocation);
        }
        let mut done = pending("done", "frame-main");
        done.status = Some("Completed".into());
        st.invocations.insert("done".into(), done);
        let noted = |st: &WebmcpState, id: &str| st.invocations.get(id).unwrap().note.is_some();
        // When the iframe navigates, then only its pending invocation is noted
        st.clear_frame_tools("frame-iframe");
        assert!(noted(&st, "p-frame") && !noted(&st, "p-main"));
        // When the page navigates, its pending one is noted, not the finished one
        st.clear_all_tools();
        assert!(noted(&st, "p-main") && !noted(&st, "done"));
        // And an answer that still comes replaces the note
        st.record_response(&json!({"invocationId": "p-main", "status": "Completed"}));
        assert!(!noted(&st, "p-main"));
    }

    /// ISSUE-73: the frame of a tool named without frameId: the only frame
    /// registering it; none or several fail naming what to do.
    #[test]
    fn a_tool_name_finds_its_frame() {
        // Given toolA in frame-main, and toolB in two frames
        let mut st = WebmcpState::default();
        seed_tools(&mut st);
        let mut twice = st.tools["frame-main"]["toolA"].clone();
        twice.name = "toolB".into();
        for frame in ["frame-main", "frame-iframe"] {
            let mut tool = twice.clone();
            tool.frame_id = frame.into();
            st.tools
                .get_mut(frame)
                .unwrap()
                .insert("toolB".into(), tool);
        }
        // Then a single one is found, the others fail naming the way out
        assert_eq!(st.frame_of("toolA").unwrap(), "frame-main");
        let ambiguous = st.frame_of("toolB").unwrap_err();
        assert!(
            ambiguous.contains("frame-iframe") && ambiguous.contains("frame-main"),
            "{ambiguous}"
        );
        assert!(ambiguous.contains("frameId"), "{ambiguous}");
        let missing = st.frame_of("nope").unwrap_err();
        assert!(missing.contains("webmcp_list_tools"), "{missing}");
    }

    #[tokio::test]
    async fn given_clear_frame_tools_then_only_that_frame_is_dropped() {
        let mut st = WebmcpState::default();
        seed_tools(&mut st);

        st.clear_frame_tools("frame-iframe");

        assert!(!st.tools.contains_key("frame-iframe"), "iframe purged");
        assert!(
            st.tools.contains_key("frame-main"),
            "main frame tools must survive a child-frame purge"
        );
    }

    #[tokio::test]
    async fn given_clear_all_tools_then_tools_go_and_invocations_stay() {
        let mut st = WebmcpState::default();
        seed_tools(&mut st);
        st.availability = WebmcpAvailability::Enabled;
        st.invocations.insert(
            "inv-1".to_string(),
            WebmcpInvocation {
                tool_name: "toolA".to_string(),
                frame_id: "frame-main".to_string(),
                invocation_id: "inv-1".to_string(),
                input: "{}".to_string(),
                status: Some("Completed".to_string()),
                output: None,
                error_text: None,
                untrusted_content: false,
                note: None,
            },
        );

        st.clear_all_tools();

        assert!(st.tools.is_empty(), "all tools purged");
        assert!(
            !st.invocations.is_empty(),
            "invocations must survive navigation"
        );
        assert_eq!(
            st.availability,
            WebmcpAvailability::Enabled,
            "availability is not touched by a purge"
        );
    }

    /// The real `toolsAdded` event of a tool declaring `consequentialHint`
    /// and `untrustedContentHint` (Chrome 156).
    fn tools_added() -> WsResponse {
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/cdp/webmcp-tools-added.json"
        ))
        .unwrap()
    }

    /// AUD-RW-17: every annotation Chrome sends reaches the agent
    /// (`consequential` and `debugging` were dropped), and one a later
    /// Chrome adds is kept too.
    #[tokio::test]
    async fn annotations_are_kept_whole() {
        let state = Arc::new(Mutex::new(WebmcpState::default()));
        let mut event = tools_added();
        if let Some(params) = event.params.as_mut() {
            params["tools"][0]["annotations"]["futureHint"] = serde_json::json!(true);
        }
        process_webmcp_event(&event, &state).await;
        let st = state.lock().await;
        let tool = &st.tools["A6A805AFA9B8063606A09E5A6AF77830"]["pay"];
        let listed = serde_json::to_value(tool).unwrap();
        assert_eq!(
            listed["annotations"],
            serde_json::json!({"consequential": true, "debugging": false, "readOnly": false,
                "untrustedContent": true, "futureHint": true})
        );
    }

    /// AUD-RW-17: an invocation of a tool declaring untrusted content is
    /// marked, so its output can be flagged.
    #[tokio::test]
    async fn invocations_of_untrusted_tools_are_marked() {
        let state = Arc::new(Mutex::new(WebmcpState::default()));
        process_webmcp_event(&tools_added(), &state).await;
        let invoked = |id: &str, tool: &str| WsResponse {
            method: Some("WebMCP.toolInvoked".into()),
            params: Some(
                serde_json::json!({"toolName": tool, "frameId": "A6A805AFA9B8063606A09E5A6AF77830",
                "invocationId": id, "input": "{}"}),
            ),
            ..Default::default()
        };
        process_webmcp_event(&invoked("i1", "pay"), &state).await;
        process_webmcp_event(&invoked("i2", "other"), &state).await;
        let st = state.lock().await;
        assert!(st.invocations["i1"].untrusted_content);
        assert!(!st.invocations["i2"].untrusted_content);
    }
}
