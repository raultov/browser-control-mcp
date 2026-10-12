use crate::handler::{BrowserSession, BrowserVersion};
use crate::util::sync::{read, write};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

pub type InstanceId = String;

#[derive(Debug, Clone, ::serde::Serialize, ::serde::Deserialize)]
pub struct InstanceDescriptor {
    pub id: InstanceId,
    /// The browser this instance runs, for its whole life.
    pub browser: crate::backend::BrowserKind,
    pub label: Option<String>,
    pub host: String,
    pub port: u16,
    pub profile_dir: Option<PathBuf>,
    pub features: Vec<String>,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_version: Option<BrowserVersion>,
}

impl InstanceDescriptor {
    /// The descriptor of the lazily launched `default` instance.
    pub fn default_instance(port: u16, features: Vec<String>) -> Self {
        Self {
            id: "default".to_string(),
            browser: crate::backend::BrowserKind::Chrome,
            label: None,
            host: "127.0.0.1".to_string(), // approximation
            port,
            profile_dir: None, // lazy
            features,
            is_default: true,
            browser_version: None,
        }
    }
}

/// Sessions and their descriptors behind **one** lock: two locks taken in
/// different orders by different operations could deadlock.
#[derive(Default)]
struct RegistryState {
    sessions: HashMap<InstanceId, Arc<BrowserSession>>,
    firefox: HashMap<InstanceId, Arc<crate::backend::bidi::session::FirefoxSession>>,
    descriptors: HashMap<InstanceId, InstanceDescriptor>,
}

pub(crate) struct Registry {
    state: RwLock<RegistryState>,
    counter: std::sync::atomic::AtomicUsize,
    pub(crate) max_instances: std::sync::atomic::AtomicUsize,
    /// Set when the server starts shutting down: no tool may launch anything.
    closing: std::sync::atomic::AtomicBool,
}

impl Registry {
    pub fn register_descriptor(&self, desc: InstanceDescriptor) {
        write(&self.state).descriptors.insert(desc.id.clone(), desc);
    }
    pub fn new(max_instances: usize) -> Self {
        Self {
            state: RwLock::new(RegistryState::default()),
            counter: std::sync::atomic::AtomicUsize::new(2), // start at 2 since "default" is 1 conceptually
            max_instances: std::sync::atomic::AtomicUsize::new(max_instances),
            closing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Marks the server as shutting down (irreversible).
    pub(crate) fn close(&self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the server is shutting down.
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn get_session(&self, id: &str) -> Option<Arc<BrowserSession>> {
        read(&self.state).sessions.get(id).cloned()
    }

    pub fn add_session(
        &self,
        desc: InstanceDescriptor,
        session: Arc<BrowserSession>,
    ) -> Result<(), String> {
        self.add_with(desc, |state, id| {
            state.sessions.insert(id, session);
        })
    }

    /// Registers a Firefox instance (same checks as Chrome ones).
    pub(crate) fn add_firefox(
        &self,
        desc: InstanceDescriptor,
        session: Arc<crate::backend::bidi::session::FirefoxSession>,
    ) -> Result<(), String> {
        self.add_with(desc, |state, id| {
            state.firefox.insert(id, session);
        })
    }

    /// Checks, records the descriptor and lets `insert` store the session,
    /// all under one lock.
    fn add_with(
        &self,
        desc: InstanceDescriptor,
        insert: impl FnOnce(&mut RegistryState, InstanceId),
    ) -> Result<(), String> {
        let mut state = write(&self.state);
        self.check_can_add(&state, &desc)?;
        let id = desc.id.clone();
        state.descriptors.insert(id.clone(), desc);
        insert(&mut state, id);
        Ok(())
    }

    /// Whether an instance like `desc` may be added: under the cap, with a
    /// new id and an unused label. Callers check before launching anything.
    pub(crate) fn can_add(&self, desc: &InstanceDescriptor) -> Result<(), String> {
        self.check_can_add(&read(&self.state), desc)
    }

    fn check_can_add(
        &self,
        state: &RegistryState,
        desc: &InstanceDescriptor,
    ) -> Result<(), String> {
        let max = self.max_instances.load(std::sync::atomic::Ordering::SeqCst);
        if state.sessions.len() + state.firefox.len() >= max {
            return Err(format!("Instance limit reached ({max})"));
        }
        // The default instance's descriptor exists before its session: only
        // the default instance itself may take that id.
        let reserved = !desc.is_default && state.descriptors.contains_key(&desc.id);
        if reserved || state.sessions.contains_key(&desc.id) || state.firefox.contains_key(&desc.id)
        {
            return Err(format!("Instance id '{}' already exists", desc.id));
        }
        if let Some(ref label) = desc.label
            && let Some(existing) = state
                .descriptors
                .values()
                .find(|d| d.label.as_ref() == Some(label))
        {
            return Err(format!(
                "Label '{}' is already in use by instance '{}'",
                label, existing.id
            ));
        }
        Ok(())
    }

    /// The Firefox session `id`, if registered.
    pub(crate) fn get_firefox(
        &self,
        id: &str,
    ) -> Option<Arc<crate::backend::bidi::session::FirefoxSession>> {
        read(&self.state).firefox.get(id).cloned()
    }

    /// Every registered Firefox session with its id.
    pub(crate) fn firefox_sessions(
        &self,
    ) -> Vec<(
        InstanceId,
        Arc<crate::backend::bidi::session::FirefoxSession>,
    )> {
        read(&self.state)
            .firefox
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// Updates a descriptor in place (e.g. port and profile once launched).
    pub(crate) fn update_descriptor(&self, id: &str, update: impl FnOnce(&mut InstanceDescriptor)) {
        if let Some(desc) = write(&self.state).descriptors.get_mut(id) {
            update(desc);
        }
    }

    pub fn remove_session(&self, id: &str) -> Option<Arc<BrowserSession>> {
        let mut state = write(&self.state);
        state.descriptors.remove(id);
        state.firefox.remove(id);
        state.sessions.remove(id)
    }

    pub fn list_descriptors(&self) -> Vec<InstanceDescriptor> {
        let state = read(&self.state);
        state
            .descriptors
            .values()
            .cloned()
            .map(|mut desc| {
                if let Some(session) = state.sessions.get(&desc.id)
                    && let Ok(guard) = session.browser_version.try_lock()
                {
                    desc.browser_version = guard.clone();
                }
                desc
            })
            .collect()
    }

    /// A fresh `<browser>-N` id (D27).
    pub fn generate_id(&self, browser: crate::backend::BrowserKind) -> InstanceId {
        let num = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{}-{}", browser.name(), num)
    }

    /// Every registered session with its id.
    pub(crate) fn sessions(&self) -> Vec<(InstanceId, Arc<BrowserSession>)> {
        read(&self.state)
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// The browser of instance `id`, if it is registered.
    pub fn browser_of(&self, id: &str) -> Option<crate::backend::BrowserKind> {
        read(&self.state).descriptors.get(id).map(|d| d.browser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::McpHandler;

    fn descriptor(id: &str) -> InstanceDescriptor {
        InstanceDescriptor {
            id: id.to_string(),
            browser: crate::backend::BrowserKind::Chrome,
            label: Some(id.to_string()),
            host: "127.0.0.1".into(),
            port: 0,
            profile_dir: None,
            features: vec![],
            is_default: false,
            browser_version: None,
        }
    }

    /// BKD-05 / BKD-04 / BKD-01: generated ids carry the browser name; an
    /// instance keeps its browser; the default one follows `--browser`.
    #[test]
    fn instance_ids_and_browsers() {
        // Given a registry
        let registry = Registry::new(8);
        // When ids are generated and a Firefox instance is registered
        let chrome = registry.generate_id(crate::backend::BrowserKind::Chrome);
        let firefox = registry.generate_id(crate::backend::BrowserKind::Firefox);
        let mut desc = descriptor(&firefox);
        desc.browser = crate::backend::BrowserKind::Firefox;
        let session = Arc::new(crate::backend::bidi::session::FirefoxSession::new(
            Default::default(),
        ));
        registry.add_firefox(desc, session).unwrap();
        // Then the ids name their browser and the instance stays Firefox
        assert!(chrome.starts_with("chrome-") && firefox.starts_with("firefox-"));
        assert_eq!(
            registry.browser_of(&firefox),
            Some(crate::backend::BrowserKind::Firefox)
        );
        use clap::Parser;
        let args = crate::Args::parse_from(["browser-control-mcp", "--browser", "firefox"]);
        let handler = crate::build_handler(args);
        assert_eq!(
            handler.browser_of(None),
            crate::backend::BrowserKind::Firefox
        );
    }

    /// BKD-09: The instance cap is checked before launching.
    #[test]
    fn instance_cap_checked_before_launch() {
        // Given a registry full at one instance
        let registry = Registry::new(1);
        let session = McpHandler::new_test().default_session.clone();
        registry.add_session(descriptor("a"), session).unwrap();
        // When another instance is planned
        let planned = registry.can_add(&descriptor("b"));
        // Then it is refused before anything is launched
        assert!(planned.is_err_and(|e| e.contains("limit")));
    }

    /// REV-04: no other instance can take the id of the default instance
    /// (registered before its browser is launched) or of any other one.
    #[test]
    fn default_and_known_ids_are_reserved() {
        // Given a registry that knows the default instance
        let registry = Registry::new(8);
        registry.register_descriptor(InstanceDescriptor::default_instance(9222, vec![]));
        let session = McpHandler::new_test().default_session.clone();
        // When an instance labelled "default" is added
        let taken = registry.add_session(descriptor("default"), session.clone());
        // Then it is refused, and the default descriptor is untouched
        assert!(taken.is_err_and(|e| e.contains("already exists")));
        assert!(registry.list_descriptors().iter().any(|d| d.is_default));
        // But the default instance itself still registers its session
        let mut default = InstanceDescriptor::default_instance(9222, vec![]);
        default.browser = crate::backend::BrowserKind::Firefox;
        let firefox = Arc::new(crate::backend::bidi::session::FirefoxSession::new(
            Default::default(),
        ));
        assert!(registry.add_firefox(default, firefox).is_ok());
    }

    /// SEC-14: Registry operations cannot deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn registry_operations_do_not_deadlock() {
        // Given a registry and a session to register
        let registry = Arc::new(Registry::new(usize::MAX));
        let session = McpHandler::new_test().default_session.clone();
        // When 8 tasks add, list and remove sessions 1,000 times concurrently
        let tasks: Vec<_> = (0..8)
            .map(|t| {
                let (registry, session) = (registry.clone(), session.clone());
                tokio::task::spawn_blocking(move || churn(&registry, &session, t))
            })
            .collect();
        // Then all complete within 5 seconds and nothing is left behind
        let all = futures_join(tasks);
        tokio::time::timeout(std::time::Duration::from_secs(5), all)
            .await
            .expect("registry operations deadlocked");
        assert!(registry.list_descriptors().is_empty());
    }

    fn churn(registry: &Registry, session: &Arc<BrowserSession>, task: usize) {
        for i in 0..1000 {
            let id = format!("i-{task}-{i}");
            let _ = registry.add_session(descriptor(&id), session.clone());
            let _ = registry.list_descriptors();
            let _ = registry.remove_session(&id);
        }
    }

    async fn futures_join(tasks: Vec<tokio::task::JoinHandle<()>>) {
        for task in tasks {
            task.await.unwrap();
        }
    }
}
