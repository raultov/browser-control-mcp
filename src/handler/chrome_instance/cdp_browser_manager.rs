use async_trait::async_trait;
use cdp_browser_lite::{BrowserClient, BrowserConfig, CdpClient};

use crate::handler::chrome_instance::cookie_seed::SeedReport;
use crate::handler::chrome_instance::launch::{LaunchParams, LaunchPlan};

pub(crate) struct CdpBrowserManager {
    pub(crate) params: LaunchParams,
    pub(crate) launcher: Box<dyn BrowserLauncher>,
    pub(crate) browser: Option<Box<dyn ManagedBrowser>>,
    pub(crate) resolved_port: u16,
    pub(crate) seed_report: Option<SeedReport>,
}

impl CdpBrowserManager {
    pub(crate) fn new(params: LaunchParams, launcher: Box<dyn BrowserLauncher>) -> Self {
        let resolved_port = params.configured_port();
        Self {
            params,
            launcher,
            browser: None,
            resolved_port,
            seed_report: None,
        }
    }
}

#[async_trait]
impl crate::handler::chrome_instance::ChromeManager for CdpBrowserManager {
    async fn ensure_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()> {
        if let Some(browser) = self.browser.as_ref() {
            if browser.is_alive().await {
                return Ok(());
            }
            // Relaunching here would hand every tab, listener and cached
            // connection a browser they do not belong to: the agent restarts.
            return Err(crate::handler::chrome_instance::manager_error!(
                "{}",
                crate::handler::BROWSER_GONE
            ));
        }
        let plan: LaunchPlan = self.params.plan();
        let config = self.params.to_config(&plan);
        let browser = self.launcher.launch(config).await?;
        self.resolved_port = browser.resolved_port().await;
        self.browser = Some(browser);
        Ok(())
    }

    async fn stop_instance(&mut self) -> crate::handler::chrome_instance::ManagerResult<()> {
        let stopped = match self.browser.take() {
            Some(browser) => browser.stop().await,
            None => Ok(()),
        };
        // The copy of the user's cookies goes whether or not the browser
        // stopped cleanly (dropping the report deletes it).
        if self.seed_report.take().is_some() {
            self.params.set_seed_profile(None);
        }
        self.resolved_port = self.params.configured_port();
        stopped
    }

    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient> {
        let browser = self.browser.as_ref().ok_or_else(|| {
            crate::handler::chrome_instance::manager_error!(
                "Chrome instance has not been ensured yet"
            )
        })?;
        browser.client().await
    }

    async fn browser_client(
        &self,
    ) -> crate::handler::chrome_instance::ManagerResult<BrowserClient> {
        let browser = self.browser.as_ref().ok_or_else(|| {
            crate::handler::chrome_instance::manager_error!(
                "Chrome instance has not been ensured yet"
            )
        })?;
        browser.browser_client().await
    }

    async fn profile_dir(&self) -> Option<std::path::PathBuf> {
        let browser = self.browser.as_ref()?;
        browser.profile_dir().await
    }

    fn get_port(&self) -> u16 {
        self.resolved_port
    }

    fn set_proxy(&mut self, proxy: Option<String>) {
        self.params.set_proxy(proxy);
    }

    fn proxy(&self) -> Option<&str> {
        self.params.proxy()
    }

    fn set_features(
        &mut self,
        features: Vec<crate::handler::chrome_instance::launch::ChromeFeature>,
    ) {
        self.params.set_features(features);
    }

    fn features(&self) -> &[crate::handler::chrome_instance::launch::ChromeFeature] {
        self.params.features()
    }

    async fn is_attached(&self) -> bool {
        match self.browser.as_ref() {
            Some(browser) => !browser.is_managed().await,
            None => false,
        }
    }

    async fn is_running(&self) -> bool {
        if let Some(browser) = self.browser.as_ref() {
            browser.is_alive().await
        } else {
            false
        }
    }

    async fn crashed(&self) -> bool {
        match self.browser.as_ref() {
            Some(browser) => !browser.is_alive().await,
            None => false,
        }
    }

    fn set_seed_profile(&mut self, seed: Option<SeedReport>) {
        if let Some(ref s) = seed {
            self.params.set_seed_profile(Some(s.dir.clone()));
        } else {
            self.params.set_seed_profile(None);
        }
        self.seed_report = seed;
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait]
pub(crate) trait ManagedBrowser: Send + Sync {
    async fn resolved_port(&self) -> u16;
    async fn is_alive(&self) -> bool;
    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient>;
    /// Retrieves a browser-level CDP client for tab management.
    /// Default implementation rejects when the backend cannot provide one.
    async fn browser_client(&self)
    -> crate::handler::chrome_instance::ManagerResult<BrowserClient>;
    /// On-disk profile directory, if the backend exposes one.
    async fn profile_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    async fn stop(&self) -> crate::handler::chrome_instance::ManagerResult<()>;
    /// Whether the browser was launched here (`false`: a Chrome already
    /// listening on the port was attached, and `stop` only disconnects).
    async fn is_managed(&self) -> bool {
        true
    }
}

#[async_trait]
pub(crate) trait BrowserLauncher: Send + Sync {
    async fn launch(
        &self,
        config: BrowserConfig,
    ) -> crate::handler::chrome_instance::ManagerResult<Box<dyn ManagedBrowser>>;
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;
}

pub(crate) struct RealBrowser {
    browser: std::sync::Arc<cdp_browser_lite::Browser>,
    id: cdp_browser_lite::BrowserId,
    pool: std::sync::Arc<cdp_browser_lite::BrowserPool>,
}

#[async_trait]
impl ManagedBrowser for RealBrowser {
    async fn resolved_port(&self) -> u16 {
        self.browser.debug_address().await.1
    }

    async fn is_alive(&self) -> bool {
        self.browser.is_alive().await
    }

    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient> {
        self.browser
            .client()
            .await
            .map_err(crate::handler::chrome_instance::ManagerError::from)
    }

    async fn browser_client(
        &self,
    ) -> crate::handler::chrome_instance::ManagerResult<BrowserClient> {
        self.browser
            .browser_client()
            .await
            .map_err(crate::handler::chrome_instance::ManagerError::from)
    }

    async fn profile_dir(&self) -> Option<std::path::PathBuf> {
        self.browser.profile_dir().await
    }

    async fn stop(&self) -> crate::handler::chrome_instance::ManagerResult<()> {
        self.pool
            .close(self.id)
            .await
            .map_err(crate::handler::chrome_instance::ManagerError::from)
    }

    async fn is_managed(&self) -> bool {
        self.browser.is_managed().await
    }
}

pub(crate) struct RealLauncher {
    pub(crate) pool: std::sync::Arc<cdp_browser_lite::BrowserPool>,
}

#[async_trait]
impl BrowserLauncher for RealLauncher {
    async fn launch(
        &self,
        config: BrowserConfig,
    ) -> crate::handler::chrome_instance::ManagerResult<Box<dyn ManagedBrowser>> {
        let id = self
            .pool
            .open(config)
            .await
            .map_err(crate::handler::chrome_instance::ManagerError::from)?;
        let browser = self.pool.get(id).await;
        let Some(browser) = browser else {
            return Err(crate::handler::chrome_instance::manager_error!(
                "browser entry missing for {:?}",
                id
            ));
        };
        Ok(Box::new(RealBrowser {
            browser,
            id,
            pool: self.pool.clone(),
        }))
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
pub(crate) struct FakeBrowser {
    pub(crate) resolved_port: u16,
    pub(crate) alive: std::sync::atomic::AtomicBool,
    pub(crate) stopped: std::sync::atomic::AtomicBool,
    pub(crate) stop_calls: std::sync::atomic::AtomicU32,
    /// Makes `stop` fail, as a browser that cannot be killed.
    pub(crate) fail_stop: std::sync::atomic::AtomicBool,
    /// Found running on the port instead of launched.
    pub(crate) attached: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl FakeBrowser {
    pub(crate) fn new(resolved_port: u16) -> Self {
        Self {
            resolved_port,
            alive: std::sync::atomic::AtomicBool::new(true),
            stopped: std::sync::atomic::AtomicBool::new(false),
            stop_calls: std::sync::atomic::AtomicU32::new(0),
            fail_stop: std::sync::atomic::AtomicBool::new(false),
            attached: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn kill(&self) {
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
#[async_trait]
impl ManagedBrowser for FakeBrowser {
    async fn resolved_port(&self) -> u16 {
        self.resolved_port
    }

    async fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient> {
        Err(crate::handler::chrome_instance::manager_error!(
            "FakeBrowser::client is not implemented; tests should not reach this path"
        ))
    }

    async fn browser_client(
        &self,
    ) -> crate::handler::chrome_instance::ManagerResult<BrowserClient> {
        Err(crate::handler::chrome_instance::manager_error!(
            "FakeBrowser::browser_client is not implemented; tests should not reach this path"
        ))
    }

    async fn stop(&self) -> crate::handler::chrome_instance::ManagerResult<()> {
        self.stop_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::handler::chrome_instance::manager_error!(
                "FakeBrowser: stop failed"
            ));
        }
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn is_managed(&self) -> bool {
        !self.attached.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
pub(crate) struct FakeLauncher {
    pub(crate) configs: std::sync::Mutex<Vec<BrowserConfig>>,
    pub(crate) next: std::sync::Mutex<std::collections::VecDeque<FakeBrowser>>,
    pub(crate) launch_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) launched: std::sync::Mutex<Vec<std::sync::Arc<FakeBrowser>>>,
}

#[cfg(test)]
impl FakeLauncher {
    pub(crate) fn new(initial: FakeBrowser) -> Self {
        Self {
            configs: std::sync::Mutex::new(Vec::new()),
            next: std::sync::Mutex::new(std::collections::VecDeque::from(vec![initial])),
            launch_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            launched: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn script(sequence: Vec<FakeBrowser>) -> Self {
        Self {
            configs: std::sync::Mutex::new(Vec::new()),
            next: std::sync::Mutex::new(std::collections::VecDeque::from(sequence)),
            launch_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            launched: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl BrowserLauncher for FakeLauncher {
    async fn launch(
        &self,
        config: BrowserConfig,
    ) -> crate::handler::chrome_instance::ManagerResult<Box<dyn ManagedBrowser>> {
        self.configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(config);
        self.launch_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| {
                crate::handler::chrome_instance::manager_error!(
                    "FakeLauncher: no scripted FakeBrowser left"
                )
            })?;
        let arc = std::sync::Arc::new(next);
        self.launched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(arc.clone());
        Ok(Box::new(FakeBrowserHandle(arc)))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
struct FakeBrowserHandle(std::sync::Arc<FakeBrowser>);

#[cfg(test)]
#[async_trait]
impl ManagedBrowser for FakeBrowserHandle {
    async fn resolved_port(&self) -> u16 {
        self.0.resolved_port().await
    }

    async fn is_alive(&self) -> bool {
        self.0.is_alive().await
    }

    async fn client(&self) -> crate::handler::chrome_instance::ManagerResult<CdpClient> {
        Err(crate::handler::chrome_instance::manager_error!(
            "FakeBrowserHandle::client is not implemented; tests should not reach this path"
        ))
    }

    async fn browser_client(
        &self,
    ) -> crate::handler::chrome_instance::ManagerResult<BrowserClient> {
        self.0.browser_client().await
    }

    async fn stop(&self) -> crate::handler::chrome_instance::ManagerResult<()> {
        self.0.stop().await
    }

    async fn is_managed(&self) -> bool {
        self.0.is_managed().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::chrome_instance::ChromeManager;
    use crate::handler::chrome_instance::launch::LaunchParams;

    fn params(port: u16) -> LaunchParams {
        LaunchParams::new(
            "127.0.0.1".into(),
            port,
            crate::cli::BrowserFlags {
                headless: true,
                ..Default::default()
            },
        )
    }

    /// AUD-LC-06: a Chrome found already running on the port (attached, not
    /// launched) is told apart from one this manager launched.
    #[tokio::test]
    async fn attached_browser_is_reported() {
        let launched = FakeBrowser::new(9222);
        let found = FakeBrowser::new(9222);
        found
            .attached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let launcher = FakeLauncher::script(vec![launched, found]);
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        assert!(!mgr.is_attached().await, "nothing ensured yet");
        mgr.ensure_instance().await.unwrap();
        assert!(!mgr.is_attached().await);
        mgr.stop_instance().await.unwrap();
        mgr.ensure_instance().await.unwrap();
        assert!(mgr.is_attached().await);
    }

    #[tokio::test]
    async fn given_no_browser_when_ensure_instance_then_launches_once() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        assert_eq!(mgr.get_port(), 9222);
    }

    #[tokio::test]
    async fn given_live_browser_when_ensure_instance_then_does_not_relaunch() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        mgr.ensure_instance().await.unwrap();
        let fake: &FakeLauncher = mgr
            .launcher
            .as_any()
            .downcast_ref::<FakeLauncher>()
            .expect("launcher must be the FakeLauncher");
        assert_eq!(
            fake.launch_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// AUD-LC-04: a browser that died is reported, not relaunched behind
    /// the agent's back (the relaunch left every tab and connection stale);
    /// stopping it (what `restart_browser` does) allows a fresh launch.
    #[tokio::test]
    async fn dead_browser_is_reported_until_stopped() {
        let launcher = FakeLauncher::script(vec![FakeBrowser::new(9222), FakeBrowser::new(9333)]);
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        assert!(!mgr.crashed().await);
        let first = {
            let fake: &FakeLauncher = mgr
                .launcher
                .as_any()
                .downcast_ref::<FakeLauncher>()
                .expect("launcher must be the FakeLauncher");
            fake.launched.lock().unwrap().first().cloned().unwrap()
        };
        first.kill();
        assert!(mgr.crashed().await);
        let error = mgr.ensure_instance().await.unwrap_err().to_string();
        assert!(error.contains("restart_browser"), "{error}");
        mgr.stop_instance().await.unwrap();
        assert!(!mgr.crashed().await);
        mgr.ensure_instance().await.unwrap();
        assert_eq!(mgr.get_port(), 9333);
    }

    #[tokio::test]
    async fn given_running_browser_when_stop_instance_then_browser_is_dropped() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        mgr.stop_instance().await.unwrap();
        assert!(mgr.browser.is_none());
    }

    #[tokio::test]
    async fn given_no_browser_when_stop_instance_then_succeeds() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.stop_instance().await.unwrap();
    }

    #[tokio::test]
    async fn given_stopped_browser_when_ensure_instance_then_launches_fresh_instance() {
        let launcher = FakeLauncher::script(vec![FakeBrowser::new(9222), FakeBrowser::new(9444)]);
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        mgr.stop_instance().await.unwrap();
        mgr.ensure_instance().await.unwrap();
        assert_eq!(mgr.get_port(), 9444);
    }

    #[tokio::test]
    async fn given_reassigned_port_when_get_port_then_returns_resolved_port() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9333));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        assert_eq!(mgr.get_port(), 9333);
    }

    #[tokio::test]
    async fn given_no_browser_when_get_port_then_returns_configured_port() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        assert_eq!(mgr.get_port(), 9222);
    }

    #[tokio::test]
    async fn given_stopped_browser_when_get_port_then_returns_configured_port() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.ensure_instance().await.unwrap();
        mgr.stop_instance().await.unwrap();
        assert_eq!(mgr.get_port(), 9222);
    }

    #[tokio::test]
    async fn given_proxy_set_when_ensure_instance_then_config_carries_proxy() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        mgr.set_proxy(Some("http://proxy.example.com:8080".into()));
        mgr.ensure_instance().await.unwrap();
    }

    /// AUD-LC-01 / AUD-EV-09: a seed replaced before use, and the seed of a
    /// browser whose stop fails, are deleted (copies of the user's cookies
    /// used to stay in the temp directory).
    #[tokio::test]
    async fn seeds_are_deleted_when_replaced_or_when_stop_fails() {
        // Given a manager seeded twice before its first launch
        let browser = FakeBrowser::new(9222);
        browser
            .fail_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut mgr = CdpBrowserManager::new(params(9222), Box::new(FakeLauncher::new(browser)));
        let (first, second) = (
            crate::handler::chrome_instance::cookie_seed::test_seed(),
            crate::handler::chrome_instance::cookie_seed::test_seed(),
        );
        let (first_dir, second_dir) = (first.dir.clone(), second.dir.clone());
        mgr.set_seed_profile(Some(first));
        mgr.set_seed_profile(Some(second));
        // Then the replaced one is gone
        assert!(!first_dir.exists());
        assert!(second_dir.exists());
        // And when the browser launched with it fails to stop
        mgr.ensure_instance().await.unwrap();
        assert!(mgr.stop_instance().await.is_err());
        // Then its seed is gone too
        assert!(!second_dir.exists());
    }

    #[tokio::test]
    async fn given_no_browser_when_client_requested_then_errors() {
        let launcher = FakeLauncher::new(FakeBrowser::new(9222));
        let mgr = CdpBrowserManager::new(params(9222), Box::new(launcher));
        let res = mgr.client().await;
        assert!(res.is_err(), "client() with no browser must error");
    }
}
