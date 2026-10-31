//! Frames of another origin on Chrome (#27, see `handler::frames`). Such a
//! frame is either out of process, with a target of its own, attached here
//! for the call, or in the page's process, reached through the tab's
//! session: script then runs on the frame's document object
//! (`Runtime.callFunctionOn`), the page's realm being out of reach.
//!
//! A [`CdpTarget`] entered into a frame ([`enter_frame`]) sends its script
//! commands (`Runtime.*`, `DOM.*`) there and its mouse events to the tab,
//! offset by where the frame shows in the page: the tools run unchanged,
//! with their checks in the frame and their input where a user's would go.

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::frames::{FrameReader, FrameTables, Route};
use cdp_browser_lite::{BrowserClient, CdpError, CdpResult, Tab, WsResponse};
use rust_mcp_sdk::schema::CallToolError;
use serde_json::{Value, json};
use std::sync::Arc;

/// Where the script of a frame runs on Chrome.
#[derive(Clone)]
pub(crate) struct FrameScope {
    /// The session: the tab's, or the frame's own target (out of process).
    session: Tab,
    /// The frame's document, when the frame shares its session with its
    /// parent (in process).
    document: Option<String>,
    /// The frame target attached for this scope (or its parent's), detached
    /// once the last clone is dropped.
    _attached: Option<Arc<Attached>>,
}

/// A frame target attached for a call: detached when dropped, so calls do
/// not pile up sessions in the browser.
struct Attached(Tab);

impl Drop for Attached {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let tab = self.0.clone();
            runtime.spawn(async move {
                let _ = tab.detach().await;
            });
        }
    }
}

impl FrameScope {
    /// The tab's page.
    pub(crate) fn page(tab: Tab) -> Self {
        Self {
            session: tab,
            document: None,
            _attached: None,
        }
    }

    /// `Runtime.evaluate` with `params` in the frame: on its document
    /// object when it has one (`expression`, `returnByValue` and
    /// `awaitPromise` are kept; the answer has the same shape).
    async fn evaluate(&self, params: Value) -> CdpResult<WsResponse> {
        let Some(document) = &self.document else {
            return self
                .session
                .send_raw_command("Runtime.evaluate", params)
                .await;
        };
        let expression = params["expression"].as_str().unwrap_or_default();
        let call = json!({
            "objectId": document,
            "functionDeclaration": format!("function() {{ return (\n{expression}\n); }}"),
            "returnByValue": params["returnByValue"].as_bool().unwrap_or(false),
            "awaitPromise": params["awaitPromise"].as_bool().unwrap_or(false),
        });
        self.session
            .send_raw_command("Runtime.callFunctionOn", call)
            .await
    }

    /// Another command, on the frame's session.
    async fn send(&self, method: &str, params: Value) -> CdpResult<WsResponse> {
        self.session.send_raw_command(method, params).await
    }

    /// The value of a snippet in the frame, or its exception.
    async fn value(&self, expression: &str) -> Result<Value, CallToolError> {
        let params = json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
        let reply = self.evaluate(params).await.map_err(failure)?;
        crate::handler::snippet_outcome(reply.result.unwrap_or_default())
    }
}

/// A CDP failure as a tool error.
fn failure(error: CdpError) -> CallToolError {
    CallToolError::from_message(crate::handler::cdp_failure("CDP Error", &error))
}

/// The error of a frame whose `<iframe>` is gone.
fn iframe_gone() -> CallToolError {
    CallToolError::from_message("The iframe is gone from the page: take a new snapshot.")
}

/// How Chrome reaches the frames of a tab's page.
pub(crate) struct ChromeFrames {
    pub(crate) page: FrameScope,
}

impl FrameReader for ChromeFrames {
    type Frame = FrameScope;

    async fn evaluate(
        &self,
        frame: Option<&FrameScope>,
        expression: &str,
    ) -> Result<Value, CallToolError> {
        frame.unwrap_or(&self.page).value(expression).await
    }

    /// The `<iframe>`'s frame id (its key), from its node: an in-process
    /// frame has its document in the node, an out-of-process one a target
    /// of that id, attached here.
    async fn child(
        &self,
        parent: Option<&FrameScope>,
        owner: &str,
        scroll: bool,
    ) -> Result<(FrameScope, String), CallToolError> {
        let scope = parent.unwrap_or(&self.page);
        let expression = crate::util::js::frame_owner(owner, scroll, "");
        let found = scope
            .evaluate(json!({"expression": expression}))
            .await
            .map_err(failure)?;
        let field = |reply: &WsResponse, pointer: &str| {
            reply
                .result
                .as_ref()
                .and_then(|r| r.pointer(pointer))
                .cloned()
        };
        let object = field(&found, "/result/objectId").ok_or_else(iframe_gone)?;
        let described = json!({"objectId": object, "depth": 1, "pierce": true});
        let node = scope
            .send("DOM.describeNode", described)
            .await
            .map_err(failure)?;
        let frame_id = field(&node, "/node/frameId")
            .and_then(|id| id.as_str().map(str::to_string))
            .ok_or_else(iframe_gone)?;
        if let Some(document) = field(&node, "/node/contentDocument/backendNodeId") {
            let resolved = scope
                .send("DOM.resolveNode", json!({"backendNodeId": document}))
                .await
                .map_err(failure)?;
            let document = field(&resolved, "/object/objectId")
                .and_then(|id| id.as_str().map(str::to_string))
                .ok_or_else(iframe_gone)?;
            let frame = FrameScope {
                document: Some(document),
                ..scope.clone()
            };
            return Ok((frame, frame_id));
        }
        let browser = BrowserClient::from_client(self.page.session.client().clone());
        let tab = browser.attach(&frame_id).await.map_err(failure)?;
        let frame = FrameScope {
            session: tab.clone(),
            document: None,
            _attached: Some(Arc::new(Attached(tab))),
        };
        Ok((frame, frame_id))
    }
}

/// A frame a [`CdpTarget`] was entered into.
pub(crate) struct FrameRoute {
    /// Where the frame's script runs.
    scope: FrameScope,
    /// Each `<iframe>` from the page down to the frame: where its script
    /// runs and its ref there.
    levels: Vec<(FrameScope, String)>,
    /// Where the frame's content shows in the page's viewport, measured at
    /// the first mouse event (after the tool scrolled its element into view).
    offset: tokio::sync::Mutex<Option<(f64, f64)>>,
}

impl FrameRoute {
    fn new(route: Route<FrameScope>, page: &FrameScope) -> Self {
        let levels = route
            .levels
            .into_iter()
            .map(|(parent, owner)| (parent.unwrap_or_else(|| page.clone()), owner))
            .collect();
        Self {
            scope: route.frame,
            levels,
            offset: tokio::sync::Mutex::default(),
        }
    }

    /// Where the frame's content starts in the page's viewport: the sum of
    /// each `<iframe>`'s content origin in its own document.
    async fn offset(&self) -> CdpResult<(f64, f64)> {
        let mut known = self.offset.lock().await;
        if let Some(offset) = *known {
            return Ok(offset);
        }
        let mut offset = (0.0, 0.0);
        for (scope, owner) in &self.levels {
            let origin = scope
                .value(&crate::util::js::frame_origin(owner))
                .await
                .ok()
                .filter(|o| o["status"] == "ok");
            let Some(origin) = origin else {
                return Err(CdpError::ProtocolError {
                    code: -32000,
                    message: "The iframe of the frame is gone from the page: take a new snapshot."
                        .into(),
                });
            };
            offset.0 += origin["x"].as_f64().unwrap_or_default();
            offset.1 += origin["y"].as_f64().unwrap_or_default();
        }
        *known = Some(offset);
        Ok(offset)
    }

    /// Sends `method` for a target entered into the frame: script commands
    /// to the frame, mouse events to `tab` offset into the frame, anything
    /// else (keys, page commands) to `tab`.
    pub(crate) async fn send(
        &self,
        tab: &Tab,
        method: &str,
        mut params: Value,
    ) -> CdpResult<WsResponse> {
        match method {
            "Runtime.evaluate" => self.scope.evaluate(params).await,
            _ if method.starts_with("Runtime.") || method.starts_with("DOM.") => {
                self.scope.send(method, params).await
            }
            "Input.dispatchMouseEvent" | "Input.dispatchDragEvent" => {
                let (x, y) = self.offset().await?;
                shift(&mut params, "x", x);
                shift(&mut params, "y", y);
                tab.send_raw_command(method, params).await
            }
            _ => tab.send_raw_command(method, params).await,
        }
    }
}

/// Adds `by` to the number `params[key]`, if there is one.
fn shift(params: &mut Value, key: &str, by: f64) {
    if let Some(at) = params[key].as_f64() {
        params[key] = json!(at + by);
    }
}

/// How Chrome reaches the frames of `target`'s tab, and the tab's key in
/// the frame tables (`None` for the browser connection, which has no tab).
pub(crate) fn frames_of(target: &CdpTarget) -> Option<(ChromeFrames, String)> {
    let tab = target.page_tab()?;
    let frames = ChromeFrames {
        page: FrameScope::page(tab.clone()),
    };
    Some((frames, tab.target_id().to_string()))
}

/// `target` entered into frame `number` of its tab (from `tables`).
pub(crate) async fn enter_frame(
    target: CdpTarget,
    tables: &FrameTables,
    number: usize,
) -> Result<CdpTarget, CallToolError> {
    let Some(tab) = target.page_tab().cloned() else {
        return Err(CallToolError::from_message(
            "Frames are reached through a tab: open one first.",
        ));
    };
    let frames = ChromeFrames {
        page: FrameScope::page(tab.clone()),
    };
    let route = crate::handler::frames::TabFrames {
        reader: &frames,
        tables,
        tab: tab.target_id(),
    }
    .enter(number)
    .await?;
    Ok(target.in_frame(FrameRoute::new(route, &frames.page)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Chrome page whose iframe `e2` (at (100, 200)) shows the in-process
    /// frame `FRAME1`, whose document object is `DOC`.
    async fn page_with_a_frame()
    -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |params| {
            let value = if params.get("returnByValue").is_none() {
                json!({"type": "object", "objectId": "IFRAME"})
            } else {
                json!({"type": "object", "value": {"status": "ok", "x": 100, "y": 200}})
            };
            json!({"result": {"result": value}})
        });
        mock.respond_with(
            "DOM.describeNode",
            |_| json!({"node": {"frameId": "FRAME1", "contentDocument": {"backendNodeId": 7}}}),
        );
        mock.respond_with(
            "DOM.resolveNode",
            |_| json!({"object": {"objectId": "DOC"}}),
        );
        mock.respond_with(
            "Runtime.callFunctionOn",
            |_| json!({"result": {"result": {"type": "number", "value": 2}}}),
        );
        mock
    }

    /// ISSUE-27: a target entered into an in-process frame (found from its
    /// iframe's node) runs script on the frame's document, and its mouse
    /// events go to the tab offset by where the frame's content starts.
    #[tokio::test]
    async fn an_in_process_frame_gets_script_and_offset_mouse_events() {
        use crate::handler::frames::FrameEntry;
        // Given frame f1 of the page, shown by the iframe e2 at (100, 200)
        let mock = page_with_a_frame().await;
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        crate::handler::cdp_domains::tests::register_tab_on(&session, mock.http_port).await;
        let page = session.target(None).await.unwrap();
        let key = page.page_tab().unwrap().target_id().to_string();
        let entry = FrameEntry {
            key: "FRAME1".into(),
            parent: 0,
            owner: "e2".into(),
        };
        session.frames.with(&key, |table| table.register(entry));
        // When a call acts on f1e5
        let target = crate::handler::resolve_target_at(&handler, None, None, Some("f1e5"))
            .await
            .unwrap();
        let value = crate::handler::snippet_value(&target, "1 + 1")
            .await
            .unwrap();
        let click = json!({"type": "mousePressed", "x": 5, "y": 6, "button": "left"});
        crate::handler::send_or(&target, "Input.dispatchMouseEvent", click, "CDP Error")
            .await
            .unwrap();
        // Then its script ran on the frame's document, marked as f1
        assert_eq!(value, json!(2));
        let calls = mock.commands_named("Runtime.callFunctionOn");
        assert!(
            calls.iter().all(|c| c.params["objectId"] == "DOC"),
            "{calls:?}"
        );
        let script: Vec<&str> = calls
            .iter()
            .filter_map(|c| c.params["functionDeclaration"].as_str())
            .collect();
        assert!(
            script[0].contains("browser-control-mcp.frame')] = 1"),
            "{script:?}"
        );
        assert!(script[1].contains("1 + 1"), "{script:?}");
        // And the mouse event landed at the frame's point in the page
        let pressed = &mock.commands_named("Input.dispatchMouseEvent")[0].params;
        assert_eq!(
            (pressed["x"].clone(), pressed["y"].clone()),
            (json!(105.0), json!(206.0))
        );
    }

    /// ISSUE-27: mouse coordinates move by the frame's offset; others stay.
    #[test]
    fn mouse_events_are_offset_into_the_frame() {
        let mut params = json!({"type": "mousePressed", "x": 10.0, "y": 20, "button": "left"});
        shift(&mut params, "x", 100.0);
        shift(&mut params, "y", 7.5);
        assert_eq!(
            (params["x"].clone(), params["y"].clone()),
            (json!(110.0), json!(27.5))
        );
        let mut keys = json!({"type": "keyDown", "key": "a"});
        shift(&mut keys, "x", 100.0);
        assert!(keys.get("x").is_none());
    }
}
