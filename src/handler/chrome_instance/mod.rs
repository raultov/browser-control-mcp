pub mod cdp_browser_manager;
pub mod close_tab;
pub(crate) mod cookie_seed;
#[cfg(test)]
pub mod integration_tests;
pub mod launch;
pub mod list_tabs;
pub mod open_tab;
pub mod restart_chrome;
pub(crate) mod seeded_relaunch;
pub mod stop_chrome;
pub mod switch_tab;
pub(crate) mod tab_lifecycle;
pub(crate) mod tab_registry;

use async_trait::async_trait;

/// Why a Chrome lifecycle operation failed (the message is shown to the agent).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ManagerError(pub(crate) String);

impl From<cdp_browser_lite::BrowserError> for ManagerError {
    fn from(error: cdp_browser_lite::BrowserError) -> Self {
        Self(error.to_string())
    }
}

impl From<cdp_browser_lite::CdpError> for ManagerError {
    fn from(error: cdp_browser_lite::CdpError) -> Self {
        Self(error.to_string())
    }
}

/// Result of a Chrome lifecycle operation.
pub(crate) type ManagerResult<T> = Result<T, ManagerError>;

/// `ManagerError` from format arguments.
macro_rules! manager_error {
    ($($arg:tt)*) => {
        $crate::handler::chrome_instance::ManagerError(format!($($arg)*))
    };
}
use cdp_browser_lite::{BrowserClient, CdpClient};
pub(crate) use manager_error;

use crate::handler::chrome_instance::cookie_seed::SeedReport;
use crate::handler::chrome_instance::launch::ChromeFeature;

#[async_trait]
pub trait ChromeManager: Send + Sync {
    async fn ensure_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()>;
    async fn stop_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()>;
    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient>;
    /// Retrieves a browser-level CDP client for tab management
    /// (open_tab, close_tab, attach_tab). Errors when the underlying
    /// manager does not support it (e.g. test mocks).
    async fn browser_client(&self)
    -> crate::handler::chrome_instance::ManagerResult<BrowserClient>;
    /// Returns the on-disk profile directory of the managed browser, if any.
    /// Defaults to `None` for managers that do not expose profile paths
    /// (e.g. test mocks). For ephemeral profiles the directory only exists
    /// while the browser is alive.
    async fn profile_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    fn get_port(&self) -> u16;
    fn set_proxy(&mut self, proxy: Option<String>);
    /// The proxy the next launch uses, so a restart can keep it. Defaults
    /// to `None` for managers that launch nothing (test doubles).
    fn proxy(&self) -> Option<&str> {
        None
    }
    /// Replaces the capability presets applied on the next launch. Like
    /// [`ChromeManager::set_proxy`], an empty list clears any previous value.
    fn set_features(&mut self, features: Vec<ChromeFeature>);
    /// Returns the currently configured capability presets.
    fn features(&self) -> &[ChromeFeature];
    /// Checks whether the underlying browser process is active and responding.
    async fn is_running(&self) -> bool {
        false
    }
    /// Sets or clears the seed profile report for the next launch.
    fn set_seed_profile(&mut self, _seed: Option<SeedReport>) {}
    /// Downcasting hook for tests (inspecting a `MockChromeManager`).
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;
}

pub struct MockChromeManager {
    port: u16,
    features: Vec<ChromeFeature>,
    proxy: Option<String>,
    running: bool,
    seed_report: Option<SeedReport>,
}

impl MockChromeManager {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            features: Vec::new(),
            proxy: None,
            running: false,
            seed_report: None,
        }
    }

    #[cfg(test)]
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    #[cfg(test)]
    pub fn is_running_mock(&self) -> bool {
        self.running
    }
}

#[async_trait]
impl ChromeManager for MockChromeManager {
    async fn ensure_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()> {
        self.running = true;
        Ok(())
    }

    async fn stop_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()> {
        self.running = false;
        self.seed_report = None;
        Ok(())
    }

    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient> {
        let addr = format!("127.0.0.1:{}", self.port);
        CdpClient::new(&addr, std::time::Duration::from_secs(10))
            .await
            .map_err(Into::into)
    }

    async fn browser_client(
        &self,
    ) -> crate::handler::chrome_instance::ManagerResult<BrowserClient> {
        Err(crate::handler::chrome_instance::manager_error!(
            "MockChromeManager does not support browser_client; tab management requires a real Chrome instance"
        ))
    }

    fn get_port(&self) -> u16 {
        self.port
    }

    fn set_proxy(&mut self, proxy: Option<String>) {
        self.proxy = proxy;
    }

    fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    fn set_features(&mut self, features: Vec<ChromeFeature>) {
        self.features = features;
    }

    fn features(&self) -> &[ChromeFeature] {
        &self.features
    }

    async fn is_running(&self) -> bool {
        self.running
    }

    fn set_seed_profile(&mut self, seed: Option<SeedReport>) {
        self.seed_report = seed;
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;

    #[tokio::test]
    async fn given_mock_manager_when_client_requested_then_connects_to_its_port() {
        let port = spawn_mock_chrome_server().await;
        let mut manager = MockChromeManager::new(port);
        assert!(!manager.is_running_mock());
        manager.set_running(true);
        assert!(manager.is_running_mock());

        let client = manager
            .client()
            .await
            .expect("mock manager client() should connect");

        let res = client
            .send_raw_command("Runtime.enable", serde_json::json!({}))
            .await;
        assert!(res.is_ok(), "command should succeed: {:?}", res.err());
    }
}
pub mod bdd_tests;
pub mod close_instance;
pub mod list_instances;
pub mod open_instance;
pub mod registry;
