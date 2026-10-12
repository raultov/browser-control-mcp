pub mod enable_proxy_auth;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::event_pump::ListenerHandles;

/// Proxy-auth handler tasks of one instance, keyed by the target they serve
/// (`None` = the session-level page). Dropping an entry aborts its task.
pub(crate) type ProxyAuthTasks = Arc<Mutex<HashMap<Option<String>, ListenerHandles>>>;
