use crate::handler::{MIN_SUPPORTED_CHROME_MAJOR, McpHandler, parse_chrome_major};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};

#[macros::mcp_tool(
    name = "list_instances",
    description = "Lists all running or registered browser instances (Chrome and Firefox). Side effects: none (read-only registry snapshot). Returns: JSON array of instance descriptors with id, browser, capabilities (what tools the instance can serve), label, host, port, profile_dir, features and is_default. Use this to discover instance_ids before passing 'instance_id' to other tools. Alternatives: 'list_tabs' to enumerate tabs within an instance."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ListInstancesTool {}

impl ListInstancesTool {
    pub async fn handle(
        _params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let descriptors = handler.registry.list_descriptors();
        let mut warnings: Vec<String> = descriptors.iter().filter_map(version_warning).collect();
        warnings.extend(firefox_warnings(handler, &descriptors));

        let mut rows = Vec::new();
        for desc in &descriptors {
            let mut row = describe(handler, desc);
            if let Some(session) = handler.registry.get_firefox(&desc.id)
                && let (Some(version), Some(map)) = (session.version().await, row.as_object_mut())
            {
                let product = format!("Firefox/{version}");
                map.insert(
                    "browser_version".into(),
                    serde_json::json!({ "product": product }),
                );
            }
            rows.push(row);
        }
        let result_json = serde_json::to_string_pretty(&rows)
            .map_err(|e| CallToolError::from_message(e.to_string()))?;

        let mut content = vec![result_json.into()];
        if !warnings.is_empty() {
            content.push(warnings.join("\n\n").into());
        }

        Ok(CallToolResult::text_content(content))
    }
}
/// CLI-05: flags that do nothing on Firefox are reported, not silently dropped.
fn firefox_warnings(
    handler: &McpHandler,
    descriptors: &[super::registry::InstanceDescriptor],
) -> Vec<String> {
    let enable_automation = handler.base_params.browser.enable_automation;
    descriptors
        .iter()
        .filter(|d| enable_automation && d.browser == crate::backend::BrowserKind::Firefox)
        .map(|d| {
            format!(
                "[Note] Instance '{}': --enable-automation is ignored on Firefox.",
                d.id
            )
        })
        .collect()
}

/// The descriptor as JSON plus the instance's capabilities.
fn describe(handler: &McpHandler, desc: &super::registry::InstanceDescriptor) -> serde_json::Value {
    let mut row = serde_json::to_value(desc).unwrap_or_default();
    let capabilities: Vec<_> = handler.capabilities(desc.browser).iter().collect();
    if let Some(map) = row.as_object_mut() {
        map.insert("capabilities".into(), serde_json::json!(capabilities));
    }
    row
}

/// A warning for an outdated browser, or a note when the product is not
/// recognised but feature presets were requested.
fn version_warning(desc: &super::registry::InstanceDescriptor) -> Option<String> {
    let product = desc.browser_version.as_ref()?.product.as_ref()?;
    match parse_chrome_major(product) {
        Some(major) if major < MIN_SUPPORTED_CHROME_MAJOR => Some(format!(
            "[Warning] Instance '{}' is running an outdated browser ({}, major {} < minimum supported {}). Some DevTools features may not work as expected.",
            desc.id, product, major, MIN_SUPPORTED_CHROME_MAJOR
        )),
        None if !desc.features.is_empty() => Some(format!(
            "[Note] Instance '{}' is running an unrecognized browser product ('{}'). Requested feature presets ({}) cannot be verified.",
            desc.id,
            product,
            desc.features.join(", ")
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BKD-11: list_instances shows browser and capabilities.
    #[tokio::test]
    async fn list_instances_shows_browser_and_capabilities() {
        // Given a Chrome default instance and a Firefox instance "ff"
        let handler = McpHandler::new_test();
        let _ = handler.session(None).await.unwrap();
        let mut ff = super::super::registry::InstanceDescriptor::default_instance(0, vec![]);
        (ff.id, ff.is_default, ff.browser) =
            ("ff".into(), false, crate::backend::BrowserKind::Firefox);
        handler.registry.register_descriptor(ff);
        // When list_instances is called
        let params = serde_json::from_value(serde_json::json!({"name": "list_instances"})).unwrap();
        let result = ListInstancesTool::handle(params, &handler).await.unwrap();
        let text = result.content[0].as_text_content().unwrap().text.clone();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        // Then each row has its browser and its sorted capabilities
        let row = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap().clone();
        assert_eq!(row("default")["browser"], "chrome");
        assert_eq!(row("ff")["browser"], "firefox");
        let ff_caps = row("ff")["capabilities"].as_array().unwrap().clone();
        assert!(ff_caps.contains(&serde_json::json!("raw_bidi")));
        assert!(!ff_caps.contains(&serde_json::json!("tracing")));
    }

    /// CLI-05: --enable-automation on Firefox is reported, not silently dropped.
    #[test]
    fn enable_automation_warns_on_firefox() {
        let mut handler = McpHandler::new_test();
        handler.base_params.browser.enable_automation = true;
        let mut ff = super::super::registry::InstanceDescriptor::default_instance(0, vec![]);
        (ff.id, ff.browser) = ("ff".into(), crate::backend::BrowserKind::Firefox);
        let warnings = firefox_warnings(&handler, &[ff]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("--enable-automation is ignored on Firefox"));
    }
}
