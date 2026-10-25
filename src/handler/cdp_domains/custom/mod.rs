pub mod get_custom_events;
pub mod send_cdp_command;

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::event_pump::{ListenerHandles, spawn_domain_listener};
use crate::handler::{CustomEvent, CustomState};
use cdp_browser_lite::WsResponse;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

/// Maximum number of buffered custom events per state (oldest dropped first).
pub(crate) const MAX_CUSTOM_EVENTS: usize = 1000;

/// Most characters of JSON kept of an event's params.
const PARAMS_MAX_CHARS: usize = 64_000;

/// `params` when their JSON fits [`PARAMS_MAX_CHARS`], else a note with its
/// length: a domain's events can carry megabytes (whole sources, bodies).
pub(crate) fn bounded_params(params: &serde_json::Value) -> serde_json::Value {
    let characters = params.to_string().chars().count();
    if characters <= PARAMS_MAX_CHARS {
        return params.clone();
    }
    serde_json::json!({"truncated": true, "characters": characters})
}

/// Stores `event` if its domain is one the agent enabled through
/// `send_cdp_command`.
pub(crate) async fn process_custom_event(event: &WsResponse, state: &Arc<Mutex<CustomState>>) {
    if let Some(method) = event.method.as_deref()
        && let Some(params) = &event.params
    {
        let mut st = state.lock().await;
        let domain = method.split('.').next().unwrap_or_default();
        if !st.active_domains.contains(domain) {
            return;
        }
        if st.events.len() >= MAX_CUSTOM_EVENTS {
            st.events.pop_front();
        }
        st.events.push_back(CustomEvent {
            method: method.to_string(),
            params: bounded_params(params),
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
    }
}

/// Records `domain` as enabled and makes sure one listener feeds `state`.
///
/// The listener receives every event of the target (empty domain filter) and
/// keeps only the enabled domains, so no `'static` domain name has to be
/// leaked. It holds only a `Weak` to `state`, which owns its handle: when the
/// tab (or session) drops the state, the handle drops and aborts the task
/// (a strong reference here used to keep both alive forever).
pub(crate) async fn ensure_domain_listener(
    target: &CdpTarget,
    state: &Arc<Mutex<CustomState>>,
    domain: &str,
) {
    let mut st = state.lock().await;
    st.active_domains.insert(domain.to_string());
    if st.handles.is_empty() {
        let weak: Weak<Mutex<CustomState>> = Arc::downgrade(state);
        let handle = spawn_domain_listener(target, "", move |event| {
            let weak = weak.clone();
            async move {
                if let Some(state) = weak.upgrade() {
                    process_custom_event(&event, &state).await;
                }
            }
        });
        st.handles.push(handle);
    }
}

pub(crate) async fn clear_custom_listeners(state: &Arc<Mutex<CustomState>>) {
    let mut st = state.lock().await;
    st.handles = ListenerHandles::default();
    st.active_domains.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-OB-13: an event's params over the limit are replaced by a note.
    #[test]
    fn large_event_params_are_replaced() {
        let small = serde_json::json!({"a": 1});
        assert_eq!(bounded_params(&small), small);
        let big = serde_json::json!({"data": "z".repeat(PARAMS_MAX_CHARS)});
        let kept = bounded_params(&big);
        assert_eq!(kept["truncated"], true);
        assert_eq!(kept["characters"], PARAMS_MAX_CHARS + 11);
    }
    use crate::handler::cdp_domains::tests::spawn_mock_chrome_server;

    async fn mock_target() -> CdpTarget {
        let port = spawn_mock_chrome_server().await;
        let address = format!("127.0.0.1:{port}");
        let timeout = std::time::Duration::from_secs(5);
        CdpTarget::Client(
            cdp_browser_lite::CdpClient::new(&address, timeout)
                .await
                .unwrap(),
        )
    }

    /// SEC-15: Closing a tab aborts its custom-domain listeners.
    #[tokio::test]
    async fn closing_a_tab_aborts_custom_listeners() {
        // Given a tab state on which send_cdp_command enabled the "Animation" domain
        let state = Arc::new(Mutex::new(CustomState::default()));
        ensure_domain_listener(&mock_target().await, &state, "Animation").await;
        let tasks = state.lock().await.handles.abort_handles();
        assert_eq!(tasks.len(), 1);
        let weak = Arc::downgrade(&state);
        // When the tab is closed (its state dropped)
        drop(state);
        // Then nothing keeps the state alive and the listener task finishes
        assert!(
            weak.upgrade().is_none(),
            "the listener still owns the state"
        );
        let finished = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !tasks.iter().all(|t| t.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(finished.is_ok(), "listener task still running");
    }

    #[tokio::test]
    async fn only_enabled_domains_are_buffered() {
        let state = Arc::new(Mutex::new(CustomState::default()));
        state.lock().await.active_domains.insert("DOM".into());
        let event =
            |m: &str| crate::handler::cdp_domains::tests::make_event(m, serde_json::json!({}));
        process_custom_event(&event("DOM.documentUpdated"), &state).await;
        process_custom_event(&event("Page.loadEventFired"), &state).await;
        let events = &state.lock().await.events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].method, "DOM.documentUpdated");
    }
}
