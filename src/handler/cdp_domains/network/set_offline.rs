//! `set_offline`: the shared arguments and reply, and the Chrome
//! implementation (`Network.emulateNetworkConditions`).

use crate::handler::McpHandler;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "set_offline",
    description = "Takes the tab offline (its requests fail as without a network, navigator.onLine is false) or back online. Lasts until set again or the tab closes; mocked routes still answer, and Chrome still serves what its HTTP cache holds (Firefox does not). Returns what the page now sees."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetOfflineTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// true = offline, false = online again.
    pub offline: bool,
}

/// What the page sees (shared by every browser).
pub(crate) const MEASURE: &str = "navigator.onLine";

impl SetOfflineTool {
    /// The reply from `navigator.onLine` as the page now reads it.
    pub(crate) fn reply(&self, online: &Value) -> CallToolResult {
        let first = if self.offline {
            "The tab is offline: its requests fail as without a network."
        } else {
            "The tab is online again."
        };
        let body = json!({"offline": self.offline, "navigator_online": online});
        let body = serde_json::to_string_pretty(&body).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: SetOfflineTool = crate::handler::parse_args(params)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        // -1: no throughput limit (only the offline switch is emulated).
        let conditions = json!({"offline": args.offline, "latency": 0,
            "downloadThroughput": -1, "uploadThroughput": -1});
        let method = "Network.emulateNetworkConditions";
        crate::handler::send_or(&target, method, conditions, "CDP Error").await?;
        let online = crate::handler::snippet_value(&target, MEASURE).await?;
        Ok(args.reply(&online))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{
        call, mock_handler, reply_text, spawn_recording_mock,
    };

    /// NET-07 (Chrome): set_offline emulates only the offline switch and
    /// reports what the page sees.
    #[tokio::test]
    async fn chrome_goes_offline() {
        // Given a tab whose page then reads navigator.onLine as false
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "boolean", "value": false}}}),
        );
        let handler = mock_handler(mock.http_port);
        // When it is taken offline
        let reply = SetOfflineTool::handle(call("set_offline", json!({"offline": true})), &handler)
            .await
            .unwrap();
        // Then Chrome emulated offline without throttling, and the reply says so
        let sent = &mock.commands_named("Network.emulateNetworkConditions")[0].params;
        assert_eq!(
            *sent,
            json!({"offline": true, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
        );
        let text = reply_text(&reply);
        assert!(text.starts_with("The tab is offline"), "{text}");
        assert!(text.contains("\"navigator_online\": false"), "{text}");
    }
}
