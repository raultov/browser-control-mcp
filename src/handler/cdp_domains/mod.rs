use crate::handler::chrome_instance::tab_registry::TabDomainStates;
use cdp_browser_lite::{NoParams, Tab};
use std::sync::Arc;
use tokio::sync::Mutex;

pub mod custom;
pub mod debugger;
pub mod fetch;
pub mod input;
pub mod network;
pub mod page;
pub mod runtime;
pub mod storage;
pub mod webmcp;

#[cfg(test)]
pub(crate) mod tests {
    use cdp_browser_lite::{CdpClient, WsResponse};
    use serde_json::json;
    use std::time::Duration;

    /// Attaches to the mock's page target and registers it as a tab of
    /// `session`; returns the tab id. The mock must outlive the test.
    pub(crate) async fn register_mock_tab(session: &crate::handler::BrowserSession) -> String {
        let port = spawn_mock_chrome_server().await;
        let address = format!("127.0.0.1:{port}");
        let browser = cdp_browser_lite::BrowserClient::connect(&address, Duration::from_secs(5))
            .await
            .expect("BrowserClient connect to mock");
        let tab = browser.attach("T-page-1").await.expect("attach to mock");
        let info = crate::handler::chrome_instance::tab_registry::NewTab::new(
            None,
            "https://example.test",
        );
        crate::util::sync::write(&session.tabs)
            .register_tab(tab, info, false)
            .expect("register tab")
    }

    pub(crate) fn make_event(method: &str, params: serde_json::Value) -> WsResponse {
        WsResponse {
            method: Some(method.to_string()),
            params: Some(params),
            ..Default::default()
        }
    }

    pub(crate) async fn spawn_mock_chrome_server() -> u16 {
        spawn_recording_mock().await.http_port
    }

    /// A mock DevTools server that lives for the whole test run, so tests
    /// can read the commands it recorded.
    pub(crate) async fn spawn_recording_mock()
    -> &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools {
        use cdp_browser_lite::test_support::mock_devtools::{MockDevTools, MockWsBehavior};
        let mock = MockDevTools::start(MockWsBehavior::StayOpen).await;
        // Leaked so it keeps running during the test
        Box::leak(Box::new(mock))
    }

    /// Emits `Page.loadEventFired` on the session of the next `command` the
    /// mock receives, as Chrome does after a navigation or a reload.
    pub(crate) fn fire_load_after(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        command: &'static str,
    ) {
        fire_after(
            mock,
            command,
            "Page.loadEventFired",
            json!({"timestamp": 1.0}),
        );
    }

    /// Emits `event` on the session of the next `command` the mock receives.
    pub(crate) fn fire_after(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        command: &'static str,
        event: &'static str,
        params: serde_json::Value,
    ) {
        fire_events_after(mock, command, vec![(event, params)]);
    }

    /// Emits `events`, in order, on the session of the next `command` the
    /// mock receives.
    pub(crate) fn fire_events_after(
        mock: &'static cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        command: &'static str,
        events: Vec<(&'static str, serde_json::Value)>,
    ) {
        let seen = mock.commands_named(command).len();
        tokio::spawn(async move {
            let Some(session) = next_command_session(mock, command, seen).await else {
                return;
            };
            for (event, params) in events {
                mock.emit(event, params, session.as_deref());
            }
        });
    }

    /// The session of the `seen`-th `command` the mock receives (within
    /// 2.5 s).
    async fn next_command_session(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        command: &str,
        seen: usize,
    ) -> Option<Option<String>> {
        for _ in 0..500 {
            if let Some(cmd) = mock.commands_named(command).get(seen) {
                return Some(cmd.session_id.clone());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        None
    }

    /// Makes the mock's current history entry `url` (the committed URL).
    pub(crate) fn committed_url(
        mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools,
        url: &'static str,
    ) {
        mock.respond_with("Page.getNavigationHistory", move |_| {
            json!({"currentIndex": 1, "entries": [{"id": 1, "url": "about:blank"}, {"id": 2, "url": url}]})
        });
    }

    /// A tool call named `name` with `arguments`.
    pub(crate) fn call(
        name: &str,
        arguments: serde_json::Value,
    ) -> rust_mcp_sdk::schema::CallToolRequestParams {
        serde_json::from_value(json!({"name": name, "arguments": arguments})).unwrap()
    }

    /// The text of a tool reply's first content block.
    pub(crate) fn reply_text(result: &rust_mcp_sdk::schema::CallToolResult) -> String {
        result.content[0].as_text_content().unwrap().text.clone()
    }

    /// A handler whose default Chrome instance talks to the mock on `port`.
    pub(crate) fn mock_handler(port: u16) -> crate::handler::McpHandler {
        let mut handler = crate::handler::McpHandler::new_test();
        std::sync::Arc::get_mut(&mut handler.default_session)
            .unwrap()
            .chrome_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::handler::chrome_instance::MockChromeManager::new(port),
        ));
        handler
    }

    #[tokio::test]
    async fn test_mock_chrome_server_connection() {
        let port = spawn_mock_chrome_server().await;
        let addr = format!("127.0.0.1:{}", port);

        let client_res = CdpClient::new(&addr, Duration::from_secs(2)).await;
        assert!(
            client_res.is_ok(),
            "Failed to connect to mock server: {:?}",
            client_res.err()
        );

        let client = client_res.unwrap();
        let res = client.send_raw_command("Runtime.enable", json!({})).await;
        assert!(res.is_ok(), "Failed to send command: {:?}", res.err());
    }

    #[tokio::test]
    async fn test_mock_chrome_server_multiple_commands() {
        let port = spawn_mock_chrome_server().await;
        let addr = format!("127.0.0.1:{}", port);

        let client = CdpClient::new(&addr, Duration::from_secs(2))
            .await
            .expect("Failed to connect");

        for i in 0..5 {
            let res = client
                .send_raw_command("Runtime.evaluate", json!({"expression": format!("{}", i)}))
                .await;
            assert!(res.is_ok(), "Command {} failed: {:?}", i, res.err());
        }
    }

    #[tokio::test]
    async fn given_enable_tab_domains_when_webmcp_enable_succeeds_then_availability_is_enabled() {
        use cdp_browser_lite::BrowserClient;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let port = spawn_mock_chrome_server().await;
        let addr = format!("127.0.0.1:{}", port);
        let browser = BrowserClient::connect(&addr, Duration::from_secs(2))
            .await
            .expect("BrowserClient connect");
        let tab = browser.attach("T-tab-1").await.expect("attach tab");
        let webmcp_state = Arc::new(Mutex::new(super::webmcp::WebmcpState {
            availability: super::webmcp::WebmcpAvailability::Pending,
            ..Default::default()
        }));

        super::enable_tab_domains(&tab, &webmcp_state).await;

        let st = webmcp_state.lock().await;
        assert_eq!(st.availability, super::webmcp::WebmcpAvailability::Enabled);
    }
}
pub(crate) mod cdp_target;
pub(crate) mod event_pump;
pub mod log;
pub mod performance;
pub mod tracing;

/// Enables the standard CDP domains on a per-tab session. Errors are ignored:
/// a missing domain only degrades that capability, it must not fail the tab.
/// The `WebMCP.enable` result is recorded in `webmcp_state.availability`.
pub(crate) async fn enable_tab_domains(tab: &Tab, webmcp_state: &Arc<Mutex<webmcp::WebmcpState>>) {
    for cmd in [
        "Runtime.enable",
        "Page.enable",
        "Network.enable",
        "Log.enable",
        "Performance.enable",
        "Debugger.enable",
    ] {
        let _ = tab.send_raw_command(cmd, NoParams).await;
    }
    // A new tab has no breakpoint: the page's `debugger;` statements are
    // skipped until one is set (`DebuggerState::skips_pauses`).
    let skip = serde_json::json!({ "skip": true });
    let _ = tab
        .send_raw_command("Debugger.setSkipAllPauses", skip)
        .await;
    let enable_res = tab.send_raw_command("WebMCP.enable", NoParams).await;
    let availability = if enable_res.is_ok() {
        webmcp::WebmcpAvailability::Enabled
    } else {
        webmcp::WebmcpAvailability::Unsupported
    };
    let mut st = webmcp_state.lock().await;
    st.availability = availability;
}

/// Starts every per-tab domain listener on the tab's CDP target.
pub(crate) fn start_tab_listeners(
    tab: &Tab,
    states: TabDomainStates,
) -> event_pump::ListenerHandles {
    let mut handles = event_pump::ListenerHandles::default();
    let target = cdp_target::CdpTarget::Tab(tab.clone());
    let TabDomainStates {
        debugger: dbg,
        network: net,
        log,
        tracing: trace,
        webmcp,
        dialog,
    } = states;
    handles.push(debugger::start_debugger_listener(&target, dbg));
    handles.push(page::start_dialog_listener(&target, dialog));
    handles.push(network::start_network_listener(&target, net.clone()));
    handles.push(page::start_page_listener(
        &target,
        net,
        Some(webmcp::WebmcpNavSync {
            state: webmcp.clone(),
            target: target.clone(),
        }),
    ));
    handles.absorb(log::start_log_listener(&target, log));
    handles.push(tracing::start_tracing_listener(&target, trace));
    handles.push(webmcp::start_webmcp_listener(&target, webmcp));
    handles
}
