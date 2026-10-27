use crate::handler::BrowserSession;
use crate::handler::chrome_instance::cookie_seed::SeedReport;
use std::sync::Arc;

pub(crate) async fn relaunch_seeded(
    session: &Arc<BrowserSession>,
    seed: SeedReport,
) -> Result<(), String> {
    // A Chrome this server did not launch cannot be relaunched seeded.
    session
        .check_not_attached()
        .await
        .map_err(|e| e.to_string())?;
    // 1. Clear cached CDP client and active tabs
    session.reset_connection_state().await;

    // 2. Stop current instance, assign seed profile, and launch fresh instance
    let mut manager = session.chrome_manager.lock().await;
    manager
        .stop_instance()
        .await
        .map_err(|e| format!("Failed to stop Chrome instance for cookie seeding: {e}"))?;

    manager.set_seed_profile(Some(seed));

    manager
        .ensure_instance()
        .await
        .map_err(|e| format!("Failed to start Chrome instance with seeded cookies: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::McpHandler;
    use crate::handler::chrome_instance::MockChromeManager;

    /// AUD-LC-06: a seeded relaunch of an attached Chrome is refused before
    /// anything is stopped.
    #[tokio::test]
    async fn attached_chrome_is_not_relaunched() {
        let mut handler = McpHandler::new_test();
        let mut manager = MockChromeManager::new(9222);
        manager.set_running(true);
        manager.set_attached(true);
        Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = Arc::new(tokio::sync::Mutex::new(manager));
        let seed = crate::handler::chrome_instance::cookie_seed::test_seed();
        let error = relaunch_seeded(&handler.default_session, seed)
            .await
            .unwrap_err();
        assert!(error.contains("did not launch"), "{error}");
        let manager = handler.default_session.chrome_manager.lock().await;
        let mock = manager
            .as_any()
            .downcast_ref::<MockChromeManager>()
            .unwrap();
        assert!(mock.is_running_mock());
    }
}
