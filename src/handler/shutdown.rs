//! Stopping every browser on every shutdown path (specs D26).

use super::BrowserSession;
use super::chrome_instance::registry::Registry;
use std::sync::Arc;
use std::time::Duration;

/// Stops the browsers of a handler after the handler itself was handed to
/// the MCP runtime.
pub(crate) struct ShutdownHook {
    pub(crate) registry: Arc<Registry>,
}

impl ShutdownHook {
    /// Stops every running instance concurrently, waiting at most `timeout`
    /// for each. Returns `(instance id, reason)` for each failure; never
    /// panics. Logs ids only.
    pub(crate) async fn stop_all(&self, timeout: Duration) -> Vec<(String, String)> {
        let mut tasks = tokio::task::JoinSet::new();
        for (id, session) in self.registry.sessions() {
            tasks.spawn(async move { stop_one(session, timeout).await.err().map(|e| (id, e)) });
        }
        let mut failures = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Some(failure)) => failures.push(failure),
                Ok(None) => {}
                Err(_) => failures.push(("<unknown>".into(), "stop task failed".into())),
            }
        }
        for (id, _) in &failures {
            tracing::warn!(instance = %id, "instance did not stop cleanly");
        }
        failures
    }
}

async fn stop_one(session: Arc<BrowserSession>, timeout: Duration) -> Result<(), String> {
    let stop = async {
        let mut manager = session.chrome_manager.lock().await;
        if manager.is_running().await {
            manager.stop_instance().await.map_err(|e| e.to_string())
        } else {
            Ok(())
        }
    };
    tokio::time::timeout(timeout, stop)
        .await
        .unwrap_or_else(|_| Err(format!("did not stop within {timeout:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::webmcp::WebmcpAvailability;
    use crate::handler::chrome_instance::launch::ChromeFeature;
    use crate::handler::chrome_instance::registry::InstanceDescriptor;
    use crate::handler::chrome_instance::{ChromeManager, MockChromeManager};
    use tokio::sync::Mutex;

    /// A manager whose stop never returns (or fails), for the unhappy paths.
    struct StuckManager {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ChromeManager for StuckManager {
        async fn ensure_instance(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop_instance(&mut self) -> anyhow::Result<()> {
            if self.fail {
                return Err(anyhow::anyhow!("boom"));
            }
            std::future::pending().await
        }
        async fn client(&self) -> anyhow::Result<cdp_browser_lite::CdpClient> {
            Err(anyhow::anyhow!("no client"))
        }
        async fn browser_client(&self) -> anyhow::Result<cdp_browser_lite::BrowserClient> {
            Err(anyhow::anyhow!("no client"))
        }
        fn get_port(&self) -> u16 {
            0
        }
        fn set_proxy(&mut self, _proxy: Option<String>) {}
        fn set_features(&mut self, _features: Vec<ChromeFeature>) {}
        fn features(&self) -> &[ChromeFeature] {
            &[]
        }
        async fn is_running(&self) -> bool {
            true
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn add(
        registry: &Registry,
        id: &str,
        manager: Arc<Mutex<dyn ChromeManager>>,
    ) -> Arc<BrowserSession> {
        let session = Arc::new(BrowserSession::new(
            manager,
            WebmcpAvailability::NotRequested,
            false,
        ));
        let mut desc = InstanceDescriptor::default_instance(0, vec![]);
        (desc.id, desc.label) = (id.into(), Some(id.into()));
        registry.add_session(desc, session.clone()).unwrap();
        session
    }

    async fn mock(running: bool) -> Arc<Mutex<dyn ChromeManager>> {
        let mut manager = MockChromeManager::new(0);
        manager.set_running(running);
        Arc::new(Mutex::new(manager))
    }

    async fn is_running(session: &BrowserSession) -> bool {
        let manager = session.chrome_manager.lock().await;
        manager
            .as_any()
            .downcast_ref::<MockChromeManager>()
            .unwrap()
            .is_running_mock()
    }

    /// SHT-01: shutdown_all stops every running instance.
    #[tokio::test]
    async fn shutdown_stops_every_running_instance() {
        // Given three instances on mocks, two running
        let registry = Arc::new(Registry::new(8));
        let a = add(&registry, "a", mock(true).await);
        let b = add(&registry, "b", mock(true).await);
        let c = add(&registry, "c", mock(false).await);
        // When the shutdown hook runs
        let hook = ShutdownHook { registry };
        let failures = hook.stop_all(Duration::from_secs(1)).await;
        // Then nothing failed and nothing is left running
        assert!(failures.is_empty(), "{failures:?}");
        for session in [a, b, c] {
            assert!(!is_running(&session).await);
        }
    }

    /// SHT-02: A hung backend does not block shutdown.
    #[tokio::test]
    async fn hung_backend_does_not_block_shutdown() {
        // Given a backend whose stop never returns and a healthy one
        let registry = Arc::new(Registry::new(8));
        add(
            &registry,
            "stuck",
            Arc::new(Mutex::new(StuckManager { fail: false })),
        );
        let healthy = add(&registry, "ok", mock(true).await);
        // When shutdown runs with a 100 ms limit
        let hook = ShutdownHook { registry };
        let started = std::time::Instant::now();
        let failures = hook.stop_all(Duration::from_millis(100)).await;
        // Then it returns quickly, reports the stuck one and stopped the healthy one
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "stuck");
        assert!(!is_running(&healthy).await);
    }

    /// SHT-04: A shutdown error is reported with the instance id, never a panic.
    #[tokio::test]
    async fn shutdown_failure_is_reported() {
        let registry = Arc::new(Registry::new(8));
        add(
            &registry,
            "broken",
            Arc::new(Mutex::new(StuckManager { fail: true })),
        );
        let failures = ShutdownHook { registry }
            .stop_all(Duration::from_secs(1))
            .await;
        assert_eq!(failures, vec![("broken".to_string(), "boom".to_string())]);
    }
}
