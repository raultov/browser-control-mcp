use super::keys::{Key, Modifier};
use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::interruption::Interruption;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::Value;

#[macros::mcp_tool(
    name = "press_key",
    description = "Presses one key with optional modifiers, as a user, in the element 'selector' names (focused first) or the focused one; the browser's default action runs (Enter submits, Tab moves the focus). With Control, Alt or Meta held the key types nothing. A dialog or breakpoint it causes is noted, as for 'click_element'."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct PressKeyTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// One character, or Enter, Tab, Escape, Backspace, Delete, ArrowLeft, ArrowUp, ArrowRight, ArrowDown, Home, End, PageUp, PageDown, Insert, Space, F1 to F12.
    pub key: String,
    /// Keys held while it is pressed.
    pub modifiers: Option<Vec<Modifier>>,
    /// Element to focus first (CSS, 'host >>> inner' or a snapshot ref). Default: the focused element.
    pub selector: Option<String>,
}

impl PressKeyTool {
    /// The key asked for, or why it is unknown.
    pub(crate) fn key(&self) -> Result<Key, CallToolError> {
        super::keys::key(&self.key).map_err(CallToolError::from_message)
    }

    /// The modifiers held.
    pub(crate) fn held(&self) -> Vec<Modifier> {
        self.modifiers.clone().unwrap_or_default()
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: PressKeyTool = crate::handler::parse_args(params)?;
        let key = args.key()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let selector = args.selector.as_deref();
        if let Some(selector) = selector {
            let focus = crate::util::js::focus(selector);
            focused(
                &crate::handler::snippet_value(&target, &focus).await?,
                selector,
            )?;
        }
        let held = args.held();
        let pressing = press(&target, &key, &held);
        let interrupted = super::keys::holding(&target, &held, pressing).await?;
        Ok(pressed_reply(selector, &key, &held, interrupted.as_ref()))
    }
}

/// Presses and releases `key` with `held` down; what interrupted it (a
/// dialog or a pause its handlers caused), if anything. A release the press
/// left unsent goes in the background, so the key is not left held.
async fn press(
    target: &CdpTarget,
    key: &Key,
    held: &[Modifier],
) -> Result<Option<Interruption>, CallToolError> {
    let method = "Input.dispatchKeyEvent";
    let [down, up] = key.cdp_events(held);
    let pressed = target.send_interruptible(method, down).await;
    let released = match pressed {
        Ok(Ok(_)) => target.send_interruptible(method, up).await,
        other => {
            let target = target.clone();
            tokio::spawn(async move { target.send_raw_command(method, up).await });
            other
        }
    };
    match released {
        Ok(Ok(_)) => Ok(None),
        Ok(Err(e)) => Err(CallToolError::from_message(crate::handler::cdp_failure(
            "CDP Error",
            &e,
        ))),
        Err(interruption) => Ok(Some(interruption)),
    }
}

/// Whether the focus snippet (`util::js::focus`) focused `selector`.
pub(crate) fn focused(value: &Value, selector: &str) -> Result<(), CallToolError> {
    match value["status"].as_str() {
        Some("focused") => Ok(()),
        Some("unfocusable") => Err(CallToolError::from_message(format!(
            "Element '{selector}' cannot take the focus; no key was pressed."
        ))),
        _ => Err(CallToolError::from_message(format!(
            "Element not found for selector: {selector}{}",
            crate::util::js::ref_hint(selector)
        ))),
    }
}

/// The reply once the key was pressed; a dialog or a pause it caused is
/// noted, as for a click.
pub(crate) fn pressed_reply(
    selector: Option<&str>,
    key: &Key,
    held: &[Modifier],
    interrupted: Option<&Interruption>,
) -> CallToolResult {
    let name = if key.key.trim().is_empty() {
        &key.code
    } else {
        &key.key
    };
    let place = selector.map_or_else(|| "the focused element".to_string(), |s| format!("'{s}'"));
    let done = format!("Pressed {}{name} in {place}", super::keys::chord(held));
    let text = interrupted.map_or_else(|| format!("{done}."), |stopped| stopped.after(&done));
    CallToolResult::text_content(vec![text.into()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A Chrome mock whose focus snippet answers `focus`.
    async fn page(
        focus: &'static str,
    ) -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            move |_| json!({"result": {"result": {"type": "object", "value": {"status": focus}}}}),
        );
        mock
    }

    async fn run(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        args: Value,
    ) -> Result<CallToolResult, CallToolError> {
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let params = crate::handler::cdp_domains::tests::call("press_key", args);
        PressKeyTool::handle(params, &handler).await
    }

    /// ISSUE-7 (Chrome): the element is focused, then the key goes down and
    /// up with the modifier bits; the reply names the chord and the place.
    #[tokio::test]
    async fn key_is_pressed_in_the_focused_element() {
        let mock = page("focused").await;
        let args = json!({"key": "Enter", "modifiers": ["Control"], "selector": "#q"});
        let reply = run(mock, args).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert_eq!(text, "Pressed Control+Enter in '#q'.");
        let events = mock.commands_named("Input.dispatchKeyEvent");
        let keys: Vec<String> = events
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e.params["type"].as_str().unwrap(),
                    e.params["key"].as_str().unwrap()
                )
            })
            .collect();
        // The modifier is pressed as a key around the key, as by a user
        assert_eq!(
            keys,
            [
                "rawKeyDown Control",
                "rawKeyDown Enter",
                "keyUp Enter",
                "keyUp Control"
            ]
        );
        assert_eq!(events[1].params["modifiers"], 2);
        assert_eq!(events[1].params["windowsVirtualKeyCode"], 13);
        assert_eq!(events[3].params["modifiers"], 0);
    }

    /// ISSUE-7: an unknown key fails before anything reaches the browser;
    /// an element that cannot take the focus gets no key.
    #[tokio::test]
    async fn unknown_keys_and_unfocusable_targets_fail() {
        let mock = page("unfocusable").await;
        let error = run(mock, json!({"key": "Hyper"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Unknown key 'Hyper'"), "{error}");
        let error = run(mock, json!({"key": "a", "selector": "#d"}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Element '#d' cannot take the focus; no key was pressed."
        );
        assert!(mock.commands_named("Input.dispatchKeyEvent").is_empty());
    }
}
