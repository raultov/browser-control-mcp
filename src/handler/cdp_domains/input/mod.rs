pub mod click_element;
pub mod drag;
pub mod fill_form;
pub mod fill_input;
pub mod hover;
pub mod keys;
pub mod press_key;
pub mod upload_file;

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::schema::CallToolError;
use serde_json::Value;

/// Why a script focus failed when the frame's Permissions-Policy refuses
/// it (Chrome 158+, `focus_blocked`, #165): a user's click is never
/// refused.
pub(crate) const FOCUS_BLOCKED: &str = "the frame's Permissions-Policy denies focus-without-user-activation; click_element the field first";

/// Whether a focusing snippet's answer (`util::js::focus`, or
/// `prepare_fill`) says the focus did not land.
fn focus_failed(value: &Value) -> bool {
    match value["status"].as_str() {
        Some("unfocusable" | "focus_blocked") => true,
        Some("not_editable") => value["unfocused"] == true,
        _ => false,
    }
}

/// The value of a focusing snippet (`util::js::focus`, `prepare_fill`) in
/// `target`. When the focus did not land in a frame of another origin
/// (whose script has no `frameElement` to focus), each `<iframe>` from the
/// page down to it is focused in its parent, as a parent may hand the
/// focus to a child frame whose Permissions-Policy refuses a script focus
/// (Chrome 158+), and the snippet runs once more (#165). No click is sent.
pub(crate) async fn focusing_snippet(
    target: &CdpTarget,
    expression: &str,
) -> Result<Value, CallToolError> {
    let value = crate::handler::snippet_value(target, expression).await?;
    if !focus_failed(&value) || !target.focus_frame_owners().await {
        return Ok(value);
    }
    crate::handler::snippet_value(target, expression).await
}
