use crate::handler::cdp_domains;
use crate::handler::chrome_instance::ChromeManager;
use crate::handler::chrome_instance::cdp_browser_manager::{CdpBrowserManager, RealLauncher};
use crate::handler::chrome_instance::launch::{ChromeFeature, LaunchParams};
use crate::handler::chrome_instance::registry::InstanceDescriptor;
use crate::handler::{BrowserSession, McpHandler};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use std::sync::Arc;
use tokio::sync::Mutex;

#[macros::mcp_tool(
    name = "open_instance",
    description = "Starts a new browser instance with its own profile, Chrome or Firefox ('browser'); its 'label' becomes the instance_id other tools take. 'features' presets: WEBGL_SOFTWARE (software WebGL for GPU-less hosts). Fails in --user-profile mode."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct OpenInstanceTool {
    /// 'chrome' (CDP) or 'firefox' (BiDi and RDP). Default: the server's --browser. Prefer Chrome unless the user asks for Firefox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserArg>,
    /// Becomes the instance_id. Default: generated.
    pub label: Option<String>,
    /// Default: the server's --headless.
    pub headless: Option<bool>,
    /// Proxy URL; credentials go to enable_proxy_auth.
    pub proxy: Option<String>,
    /// Presets: WEBGL_SOFTWARE (software WebGL).
    pub features: Option<Vec<ChromeFeature>>,
    /// Import cookies from the user's Chrome profile (needs --allow-cookie-import; Chrome only).
    pub copy_cookies: Option<bool>,
    /// Chrome profile to import cookies from (e.g. 'Profile 1'). Default: the last used.
    pub source_profile: Option<String>,
}

/// The `browser` argument (schema twin of [`crate::backend::BrowserKind`]).
#[derive(Debug, Clone, Copy, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub enum BrowserArg {
    /// Chrome or Chromium over CDP.
    #[serde(rename = "chrome")]
    Chrome,
    /// Firefox over WebDriver BiDi, with the RDP debugger.
    #[serde(rename = "firefox")]
    Firefox,
}

impl OpenInstanceTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: OpenInstanceTool = crate::handler::parse_args(params)?;
        // Chrome's singleton profile lock: --user-profile allows one instance.
        if handler.base_params.browser.user_profile
            && !handler.registry.list_descriptors().is_empty()
        {
            return Err(CallToolError::from_message(
                "Cannot open multiple instances in --user-profile mode due to Chrome's profile lock",
            ));
        }
        let instance_id = match &args.label {
            Some(label) => label.clone(),
            None => handler
                .registry
                .generate_id(crate::backend::BrowserKind::Chrome),
        };
        // Checked before launching: a rejected instance must not leave a
        // browser running that nothing tracks.
        let mut planned = InstanceDescriptor::default_instance(0, Vec::new());
        (planned.id, planned.label, planned.is_default) =
            (instance_id.clone(), args.label.clone(), false);
        handler
            .registry
            .can_add(&planned)
            .map_err(CallToolError::from_message)?;
        let child_params = args.launch_params(handler);
        let seed_report = super::cookie_seed::prepare_seed_report(
            args.copy_cookies == Some(true),
            args.source_profile.as_deref(),
            handler.flags.allow_cookie_import,
            handler.base_params.browser.user_profile,
        )
        .map_err(CallToolError::from_message)?;
        let session = Arc::new(BrowserSession::new(
            new_manager(handler, &child_params, seed_report),
            cdp_domains::webmcp::WebmcpAvailability::NotRequested,
            handler.flags.enable_webmcp,
        ));
        let desc = launch(&session, instance_id, &args, &child_params).await?;
        if let Err(e) = handler.registry.add_session(desc.clone(), session.clone()) {
            // Lost a race with a concurrent open_instance.
            let _ = session.chrome_manager.lock().await.stop_instance().await;
            return Err(CallToolError::from_message(e));
        }
        let reply = serde_json::json!({
            "instance_id": desc.id,
            "host": desc.host,
            "port": desc.port,
            "profile_dir": desc.profile_dir,
        });
        Ok(CallToolResult::text_content(vec![reply.to_string().into()]))
    }

    /// Whether the new instance runs headless: the `headless` argument, else
    /// the server's --headless flag (a window cannot open without a display).
    pub(crate) fn headless_for(&self, handler: &McpHandler) -> bool {
        self.headless
            .unwrap_or(handler.base_params.browser.headless)
    }

    /// Launch parameters of a secondary instance: port 0 (ephemeral,
    /// coordinated by the pool), no automation infobar, never the user
    /// profile (it must allow multiple profile dirs).
    fn launch_params(&self, handler: &McpHandler) -> LaunchParams {
        let browser = crate::cli::BrowserFlags {
            headless: self.headless_for(handler),
            enable_automation: false,
            user_profile: false,
            window_size: handler.base_params.browser.window_size,
        };
        let mut params = LaunchParams::new("127.0.0.1".into(), 0, browser);
        params.secondary = true;
        params.set_enable_webmcp(handler.flags.enable_webmcp);
        if let Some(features) = self.features.clone() {
            params.set_features(features);
        }
        let proxy = self
            .proxy
            .clone()
            .or_else(|| handler.proxy.proxy_server.clone());
        if proxy.is_some() {
            params.set_proxy(proxy);
        }
        params
    }
}

fn new_manager(
    handler: &McpHandler,
    params: &LaunchParams,
    seed_report: Option<super::cookie_seed::SeedReport>,
) -> Arc<Mutex<dyn ChromeManager>> {
    if handler.is_test {
        let mock_port = 9000 + handler.registry.list_descriptors().len() as u16;
        let mut mock_mgr = crate::handler::chrome_instance::MockChromeManager::new(mock_port);
        mock_mgr.set_features(params.features().to_vec());
        mock_mgr.set_seed_profile(seed_report);
        return Arc::new(Mutex::new(mock_mgr));
    }
    let launcher = Box::new(RealLauncher {
        pool: handler.pool.clone(),
    });
    let mut manager = CdpBrowserManager::new(params.clone(), launcher);
    manager.set_seed_profile(seed_report);
    Arc::new(Mutex::new(manager))
}

/// Launches the instance and describes it with its real port and profile
/// directory (ephemeral profiles get a random path at launch time).
async fn launch(
    session: &BrowserSession,
    instance_id: String,
    args: &OpenInstanceTool,
    params: &LaunchParams,
) -> Result<InstanceDescriptor, CallToolError> {
    let mut mgr = session.chrome_manager.lock().await;
    mgr.ensure_instance().await.map_err(|e| {
        CallToolError::from_message(format!(
            "Failed to spawn Chrome instance: {}",
            super::launch::describe_launch_failure(&e.to_string())
        ))
    })?;
    Ok(InstanceDescriptor {
        id: instance_id,
        browser: crate::backend::BrowserKind::Chrome,
        label: args.label.clone(),
        host: "127.0.0.1".to_string(),
        port: mgr.get_port(),
        profile_dir: mgr.profile_dir().await,
        features: params
            .features()
            .iter()
            .map(|f| f.as_name().to_string())
            .collect(),
        is_default: false,
        browser_version: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-LC-30: without `headless`, a new instance follows the server's
    /// --headless flag (it opened a window even with --headless, which fails
    /// where there is no display, e.g. the Docker image).
    #[test]
    fn headless_defaults_to_the_server_flag() {
        let mut handler = McpHandler::new_test();
        let args = |value: serde_json::Value| -> OpenInstanceTool {
            serde_json::from_value(value).unwrap()
        };
        handler.base_params.browser.headless = true;
        assert!(
            args(serde_json::json!({}))
                .launch_params(&handler)
                .browser
                .headless
        );
        assert!(args(serde_json::json!({})).headless_for(&handler));
        let shown = args(serde_json::json!({"headless": false}));
        assert!(!shown.launch_params(&handler).browser.headless);
        handler.base_params.browser.headless = false;
        assert!(!args(serde_json::json!({})).headless_for(&handler));
        assert!(args(serde_json::json!({"headless": true})).headless_for(&handler));
    }

    /// TOL-04: open_instance exposes the browser selector.
    #[test]
    fn open_instance_schema_has_browser_enum() {
        let tool = crate::handler::schema_compat::normalize_tool_with_options(
            OpenInstanceTool::tool(),
            true,
        );
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let browser = &schema["properties"]["browser"];
        assert_eq!(browser["type"], "string", "{browser}");
        assert_eq!(
            browser["enum"],
            serde_json::json!(["chrome", "firefox"]),
            "{browser}"
        );
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&serde_json::json!("browser")));
    }
}
