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
    pub label: Option<String>,
    pub host: String,
    pub port: u16,
    pub profile_dir: Option<PathBuf>,
    pub features: Vec<String>,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_version: Option<BrowserVersion>,
}

/// Sessions and their descriptors behind **one** lock: two locks taken in
/// different orders by different operations could deadlock.
#[derive(Default)]
struct RegistryState {
    sessions: HashMap<InstanceId, Arc<BrowserSession>>,
    descriptors: HashMap<InstanceId, InstanceDescriptor>,
}

pub(crate) struct Registry {
    state: RwLock<RegistryState>,
    counter: std::sync::atomic::AtomicUsize,
    pub(crate) max_instances: std::sync::atomic::AtomicUsize,
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
        }
    }

    pub fn get_session(&self, id: &str) -> Option<Arc<BrowserSession>> {
        read(&self.state).sessions.get(id).cloned()
    }

    pub fn add_session(
        &self,
        desc: InstanceDescriptor,
        session: Arc<BrowserSession>,
    ) -> Result<(), String> {
        let mut state = write(&self.state);
        let max = self.max_instances.load(std::sync::atomic::Ordering::SeqCst);
        if state.sessions.len() >= max {
            return Err(format!("Instance limit reached ({max})"));
        }
        if state.sessions.contains_key(&desc.id) {
            return Err(format!("Instance id '{}' already exists", desc.id));
        }
        // Also check if label is already used by another instance
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
        state.descriptors.insert(desc.id.clone(), desc.clone());
        state.sessions.insert(desc.id, session);
        Ok(())
    }

    pub fn remove_session(&self, id: &str) -> Option<Arc<BrowserSession>> {
        let mut state = write(&self.state);
        state.descriptors.remove(id);
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

    pub fn generate_id(&self) -> InstanceId {
        let num = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("chrome-{}", num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::McpHandler;

    fn descriptor(id: &str) -> InstanceDescriptor {
        InstanceDescriptor {
            id: id.to_string(),
            label: Some(id.to_string()),
            host: "127.0.0.1".into(),
            port: 0,
            profile_dir: None,
            features: vec![],
            is_default: false,
            browser_version: None,
        }
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
                tokio::task::spawn_blocking(move || {
                    for i in 0..1000 {
                        let id = format!("i-{t}-{i}");
                        let _ = registry.add_session(descriptor(&id), session.clone());
                        let _ = registry.list_descriptors();
                        let _ = registry.remove_session(&id);
                    }
                })
            })
            .collect();
        // Then all complete within 5 seconds and nothing is left behind
        let all = futures_join(tasks);
        tokio::time::timeout(std::time::Duration::from_secs(5), all)
            .await
            .expect("registry operations deadlocked");
        assert!(registry.list_descriptors().is_empty());
    }

    async fn futures_join(tasks: Vec<tokio::task::JoinHandle<()>>) {
        for task in tasks {
            task.await.unwrap();
        }
    }
}
