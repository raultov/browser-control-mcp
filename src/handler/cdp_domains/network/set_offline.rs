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
    description = "Takes the tab offline (its requests fail as without a network, navigator.onLine is false) or back online. Lasts until set again or the tab closes. Chrome still answers mocked routes and serves what its HTTP cache holds; Firefox fails both. Returns what the page now sees.",
    destructive_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SetOfflineTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
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
        let session = handler.session(args.instance_id.clone()).await?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        // The switch goes with the network throttling `emulate` set, in
        // one command that replaces both (#231); held as `emulate` holds it.
        let state = session.emulation_state(args.tab_id.clone())?;
        let mut emulated = state.lock().await;
        let switched = emulated.with_offline(args.offline);
        let method = "Network.emulateNetworkConditions";
        let conditions = switched.network_conditions();
        crate::handler::send_or(&target, method, conditions, "CDP Error").await?;
        *emulated = switched;
        drop(emulated);
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
            json!({"offline": true, "latency": 0.0, "downloadThroughput": -1.0, "uploadThroughput": -1.0})
        );
        let text = reply_text(&reply);
        assert!(text.starts_with("The tab is offline"), "{text}");
        assert!(text.contains("\"navigator_online\": false"), "{text}");
    }

    /// #231 (Chrome): the offline switch goes with the tab's network
    /// throttling in one command: going offline and back keeps the
    /// throttling `emulate` set, and the tab is kept offline for `emulate`.
    #[tokio::test]
    async fn chrome_keeps_the_throttling() {
        use crate::handler::cdp_domains::page::emulate::EmulateTool;
        // Given a tab throttled to slow 3G
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "object", "value": {}}}}),
        );
        let handler = mock_handler(mock.http_port);
        let slow = json!({"network_throttling": "slow-3g"});
        EmulateTool::handle(call("emulate", slow), &handler)
            .await
            .unwrap();
        // When it goes offline and back online
        for offline in [true, false] {
            let args = json!({"offline": offline});
            SetOfflineTool::handle(call("set_offline", args), &handler)
                .await
                .unwrap();
        }
        // Then each switch kept the throttling
        let sent = mock.commands_named("Network.emulateNetworkConditions");
        let switched: Vec<_> = sent[1..]
            .iter()
            .map(|c| (c.params["offline"].clone(), c.params["latency"].clone()))
            .collect();
        assert_eq!(
            switched,
            [(json!(true), json!(2000.0)), (json!(false), json!(2000.0))]
        );
        // And the tab's emulation knows it is online
        let session = handler.session(None).await.unwrap();
        let state = session.emulation_state(None).unwrap();
        assert_eq!(state.lock().await.network_conditions()["offline"], false);
    }
}
