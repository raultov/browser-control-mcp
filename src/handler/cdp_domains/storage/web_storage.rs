use crate::handler::McpHandler;
use crate::util::js::STORAGE_MAX_ITEMS;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "web_storage",
    description = "Gets, sets, deletes or clears the localStorage or sessionStorage ('kind') of the page's origin. get hides values (value_length) unless show_values; at most 200 items, values cut at 2000 characters."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct WebStorageTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// get, set, delete or clear.
    pub action: StorageAction,
    /// 'local' (default) or 'session'.
    pub kind: Option<StorageKind>,
    /// Item key: required for set and delete; for get, only this item.
    pub key: Option<String>,
    /// Value (set).
    pub value: Option<String>,
    /// Return the values (get). Default: false.
    pub show_values: Option<bool>,
}

/// The `action` of `web_storage`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum StorageAction {
    /// List items.
    #[serde(rename = "get")]
    Get,
    /// Set one item.
    #[serde(rename = "set")]
    Set,
    /// Remove one item.
    #[serde(rename = "delete")]
    Delete,
    /// Remove every item.
    #[serde(rename = "clear")]
    Clear,
}

/// The `kind` of `web_storage`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    ::serde::Deserialize,
    ::serde::Serialize,
    macros::JsonSchema,
)]
pub enum StorageKind {
    /// `localStorage`.
    #[default]
    #[serde(rename = "local")]
    Local,
    /// `sessionStorage`.
    #[serde(rename = "session")]
    Session,
}

impl WebStorageTool {
    fn action_name(&self) -> &'static str {
        match self.action {
            StorageAction::Get => "get",
            StorageAction::Set => "set",
            StorageAction::Delete => "delete",
            StorageAction::Clear => "clear",
        }
    }

    fn store(&self) -> &'static str {
        match self.kind.unwrap_or_default() {
            StorageKind::Local => "localStorage",
            StorageKind::Session => "sessionStorage",
        }
    }

    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        let fail = |message: &str| Err(CallToolError::from_message(message));
        match self.action {
            StorageAction::Set if self.key.is_none() || self.value.is_none() => {
                fail("'set' needs 'key' and 'value'")
            }
            StorageAction::Delete if self.key.is_none() => fail("'delete' needs 'key'"),
            _ => Ok(()),
        }
    }

    /// The JavaScript of the call (shared by every browser).
    pub(crate) fn expression(&self) -> String {
        let kind = match self.kind.unwrap_or_default() {
            StorageKind::Local => "local",
            StorageKind::Session => "session",
        };
        crate::util::js::web_storage(
            self.action_name(),
            kind,
            self.key.as_deref(),
            self.value.as_deref(),
        )
    }

    /// The reply from the snippet's outcome (shared by every browser).
    pub(crate) fn reply(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        let store = self.store();
        let origin = outcome["origin"].as_str().unwrap_or_default();
        let key = self.key.as_deref().unwrap_or_default();
        let text = match self.action {
            StorageAction::Get => return Ok(self.listed(outcome)),
            StorageAction::Set => format!("Set '{key}' in the {store} of {origin}."),
            StorageAction::Delete if outcome["had"] == true => {
                format!("Deleted '{key}' from the {store} of {origin}.")
            }
            StorageAction::Delete => format!("'{key}' was not in the {store} of {origin}."),
            StorageAction::Clear => format!(
                "Cleared {} item(s) from the {store} of {origin}.",
                outcome["cleared"].as_u64().unwrap_or(0)
            ),
        };
        Ok(CallToolResult::text_content(vec![text.into()]))
    }

    fn listed(&self, outcome: &Value) -> CallToolResult {
        let show = self.show_values.unwrap_or(false);
        let items: Vec<Value> = outcome["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let mut out = json!({"key": item[0]});
                        if show {
                            out["value"] = item[1].clone();
                        } else {
                            out["value_length"] = item[2].clone();
                        }
                        out
                    })
                    .collect()
            })
            .unwrap_or_default();
        let total = outcome["total"].as_u64().unwrap_or(0);
        let origin = outcome["origin"].as_str().unwrap_or_default();
        let mut first = format!("{total} item(s) in the {} of {origin}", self.store());
        if total > STORAGE_MAX_ITEMS as u64 {
            first.push_str(&format!(" (showing {STORAGE_MAX_ITEMS})"));
        }
        if !show && total > 0 {
            first.push_str("; values hidden (show_values: true returns them)");
        }
        let body = serde_json::to_string_pretty(&json!({ "items": items })).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first}.\n{body}").into()])
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        crate::handler::handle_snippet_tool::<Self>(params, handler).await
    }
}

impl crate::handler::SnippetTool for WebStorageTool {
    fn target_args(&self) -> crate::handler::TargetArgs {
        crate::handler::TargetArgs {
            instance_id: self.instance_id.clone(),
            tab_id: self.tab_id.clone(),
        }
    }

    fn check(&self) -> Result<(), CallToolError> {
        self.check_arguments()
    }

    fn snippet(&self) -> String {
        self.expression()
    }

    fn answer(&self, outcome: &Value) -> Result<CallToolResult, CallToolError> {
        self.reply(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(args: Value) -> WebStorageTool {
        serde_json::from_value(args).unwrap()
    }

    fn text(result: &CallToolResult) -> String {
        crate::handler::cdp_domains::tests::reply_text(result)
    }

    /// ISSUE-8: values stay hidden unless asked for, with their length.
    #[test]
    fn values_are_hidden_unless_asked() {
        let outcome =
            json!({"origin": "https://a.test", "total": 1, "items": [["token", "eyJhbGc", 7]]});
        let hidden = text(&tool(json!({"action": "get"})).reply(&outcome).unwrap());
        assert!(
            hidden.starts_with("1 item(s) in the localStorage of https://a.test; values hidden"),
            "{hidden}"
        );
        assert!(
            !hidden.contains("eyJhbGc") && hidden.contains("\"value_length\": 7"),
            "{hidden}"
        );
        let shown = text(
            &tool(json!({"action": "get", "kind": "session", "show_values": true}))
                .reply(&outcome)
                .unwrap(),
        );
        assert!(
            shown.starts_with("1 item(s) in the sessionStorage")
                && shown.contains("\"value\": \"eyJhbGc\""),
            "{shown}"
        );
    }

    /// ISSUE-8: changes say what changed; set and delete need a key.
    #[test]
    fn changes_are_reported() {
        let origin = |extra: Value| {
            let mut o = json!({"origin": "https://a.test"});
            o.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            o
        };
        let set = tool(json!({"action": "set", "key": "theme", "value": "dark"}));
        assert_eq!(
            text(&set.reply(&origin(json!({}))).unwrap()),
            "Set 'theme' in the localStorage of https://a.test."
        );
        let gone = tool(json!({"action": "delete", "key": "x"}));
        assert_eq!(
            text(&gone.reply(&origin(json!({"had": false}))).unwrap()),
            "'x' was not in the localStorage of https://a.test."
        );
        let clear = tool(json!({"action": "clear"}));
        assert_eq!(
            text(&clear.reply(&origin(json!({"cleared": 3}))).unwrap()),
            "Cleared 3 item(s) from the localStorage of https://a.test."
        );
        assert!(
            tool(json!({"action": "set", "key": "a"}))
                .check_arguments()
                .is_err()
        );
        assert!(tool(json!({"action": "delete"})).check_arguments().is_err());
    }

    /// ISSUE-8: the key and value reach JavaScript as literals.
    #[test]
    fn values_are_literals() {
        let crafted = r#"x"); localStorage.clear(); ("#;
        let expression =
            tool(json!({"action": "set", "key": crafted, "value": crafted})).expression();
        assert!(
            expression.contains(&crate::util::js::string_literal(crafted)),
            "{expression}"
        );
        assert!(
            expression.contains("const action = \"set\""),
            "{expression}"
        );
    }
}
