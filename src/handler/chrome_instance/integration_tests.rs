use serde_json::json;
use std::time::Duration;

use crate::handler::chrome_instance::ChromeManager;
use crate::handler::chrome_instance::cdp_browser_manager::{CdpBrowserManager, RealLauncher};
use crate::handler::chrome_instance::launch::LaunchParams;
use rust_mcp_sdk::schema::{CallToolResult, ContentBlock};

trait ContentBlockExt {
    fn as_text(&self) -> Option<&str>;
}

impl ContentBlockExt for ContentBlock {
    fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::TextContent(t) => Some(&t.text),
            _ => None,
        }
    }
}

// These tests exercise the real Chrome lifecycle against an installed Chrome
// binary (headless). They are `#[ignore]`d because they require a working
// Chrome installation and can spawn processes; run them with:
//
//     cargo test -- --ignored
//
// Status (verified on Chrome 151, `cdp-browser-lite` 0.3.2):
//
// * B2 (attach to a user-started Chrome) PASSES — upstream 0.2.2 fixed
//   `probe::is_chrome_cdp` to send the readiness probe over HTTP/1.1
//   (Chrome >= 151 ignores HTTP/1.0 requests entirely).
//
// * B3 (managed-instance handling under ephemeral profiles) PASSES — the
//   default profile mode is now `Ephemeral`, so there is no stable
//   per-port profile path: a second manager on an occupied port attaches to
//   the existing CDP endpoint instead of launching a new instance. Attached
//   instances are never killed by `stop_instance`, and the ephemeral profile
//   directory is removed when a managed instance stops.

const TEST_PORT_BASE: u16 = 19222;
const TIMEOUT_30S: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

fn local_params(port: u16) -> LaunchParams {
    LaunchParams::new(
        "127.0.0.1".into(),
        port,
        crate::cli::BrowserFlags {
            headless: true,
            ..Default::default()
        },
    )
}

async fn wait_for_port_close(host: &str, port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        let probe = async {
            let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() else {
                return false;
            };
            tokio::net::TcpStream::connect(&addr).await.is_ok()
        };
        if !probe.await {
            return true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    false
}

/// HTTP/1.1 readiness probe — works on Chrome >= 151 (see NOTE above).
async fn wait_for_cdp_ready(host: &str, port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if probe_version(host, port).await {
            return true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    false
}

/// One `GET /json/version`; true when the reply looks like Chrome's.
async fn probe_version(host: &str, port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = tokio::net::TcpStream::connect(&addr).await else {
        return false;
    };
    let req =
        format!("GET /json/version HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let looks_like_chrome = |r: &str| r.contains("Browser") || r.contains("WebKit-Version");
    let mut response = String::new();
    let mut buf = [0u8; 1024];
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 || response.len() > 64 * 1024 {
            break;
        }
        response.push_str(&String::from_utf8_lossy(&buf[..n]));
        if looks_like_chrome(&response) {
            return true;
        }
    }
    looks_like_chrome(&response)
}

#[tokio::test]
#[ignore = "requires a real Chrome installation"]
async fn given_real_chrome_when_ensure_and_client_then_browser_version_responds() {
    let port = TEST_PORT_BASE;
    let mut mgr = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    mgr.ensure_instance()
        .await
        .expect("ensure_instance must succeed against real Chrome");
    assert!(
        wait_for_cdp_ready("127.0.0.1", mgr.get_port(), TIMEOUT_30S).await,
        "managed Chrome must be reachable over HTTP/1.1 CDP"
    );
    let client = mgr.client().await.expect("client() must succeed");
    let _ = client
        .send_raw_command("Runtime.enable", cdp_browser_lite::NoParams)
        .await;
}

#[tokio::test]
#[ignore = "requires a real Chrome installation"]
async fn given_managed_instance_when_stop_instance_then_process_is_gone() {
    let port = TEST_PORT_BASE + 1;
    let mut mgr = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    mgr.ensure_instance()
        .await
        .expect("ensure_instance must succeed");
    assert!(
        wait_for_cdp_ready("127.0.0.1", mgr.get_port(), TIMEOUT_30S).await,
        "managed Chrome must be reachable before stop"
    );
    let active_port = mgr.get_port();
    mgr.stop_instance()
        .await
        .expect("stop_instance must succeed");
    assert!(
        wait_for_port_close("127.0.0.1", active_port, TIMEOUT_30S).await,
        "managed Chrome process must be terminated within {TIMEOUT_30S:?}"
    );
}

#[tokio::test]
#[ignore = "requires a real Chrome installation; covers D1 / B16"]
async fn given_managed_instance_when_manager_dropped_then_process_is_gone() {
    let port = TEST_PORT_BASE + 2;
    let mut mgr = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    mgr.ensure_instance()
        .await
        .expect("ensure_instance must succeed");
    assert!(
        wait_for_cdp_ready("127.0.0.1", mgr.get_port(), TIMEOUT_30S).await,
        "managed Chrome must be reachable before drop"
    );
    let active_port = mgr.get_port();
    drop(mgr);
    assert!(
        wait_for_port_close("127.0.0.1", active_port, TIMEOUT_30S).await,
        "dropping the manager must terminate the managed Chrome within {TIMEOUT_30S:?} (D1)"
    );
}

#[tokio::test]
#[ignore = "requires a real Chrome installation; covers B2"]
async fn given_attached_instance_when_stop_instance_then_process_survives() {
    let port = TEST_PORT_BASE + 3;
    let mut user_chrome = spawn_user_chrome_on(port);
    assert!(
        wait_for_cdp_ready("127.0.0.1", port, TIMEOUT_30S).await,
        "user-spawned Chrome must be CDP-ready on port {port}"
    );
    let mut mgr = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    mgr.ensure_instance()
        .await
        .expect("ensure_instance must attach to user Chrome");
    assert_eq!(
        mgr.get_port(),
        port,
        "manager must report the user Chrome's port (B2)"
    );
    mgr.stop_instance()
        .await
        .expect("stop_instance must succeed");
    assert!(
        wait_for_cdp_ready("127.0.0.1", port, Duration::from_secs(1)).await,
        "attached Chrome must still be CDP-ready after stop (B2)"
    );
    let _ = user_chrome.kill().await;
}

fn spawn_user_chrome_on(port: u16) -> tokio::process::Child {
    let path = cdp_browser_lite::discovery::discover_default()
        .unwrap_or_else(|_| std::path::PathBuf::from("google-chrome"));
    let _ = std::fs::remove_dir_all(format!("/tmp/browser-control-mcp-attached-test-{port}"));
    tokio::process::Command::new(&path)
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-session-crashed-bubble")
        .arg("--noerrdialogs")
        .arg("--disable-dev-shm-usage")
        .arg("--headless=new")
        .arg(format!(
            "--user-data-dir=/tmp/browser-control-mcp-attached-test-{port}"
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning user-style Chrome must succeed")
}

#[tokio::test]
#[ignore = "requires a real Chrome installation; covers B3"]
async fn given_managed_instance_on_configured_port_when_second_manager_ensures_then_attaches_to_existing_browser()
 {
    let port = TEST_PORT_BASE + 4;
    let mut first = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    first
        .ensure_instance()
        .await
        .expect("first ensure must succeed");
    let first_port = first.get_port();

    let mut second = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    second
        .ensure_instance()
        .await
        .expect("second ensure must succeed");
    assert_eq!(
        second.get_port(),
        first_port,
        "with ephemeral profiles a second manager must attach to the existing CDP endpoint"
    );

    // Attached instances are never killed: stopping the second manager must
    // leave the first manager's browser alive.
    second.stop_instance().await.unwrap();
    assert!(
        wait_for_cdp_ready("127.0.0.1", first_port, TIMEOUT_30S).await,
        "first browser must survive the attached manager's stop"
    );

    let _ = first.stop_instance().await;
}

#[tokio::test]
#[ignore = "requires a real Chrome installation; covers ephemeral profile cleanup"]
async fn given_managed_instance_when_stopped_then_ephemeral_profile_dir_is_removed() {
    let port = TEST_PORT_BASE + 5;
    let mut mgr = CdpBrowserManager::new(
        local_params(port),
        Box::new(RealLauncher {
            pool: std::sync::Arc::new(cdp_browser_lite::BrowserPool::new()),
        }),
    );
    mgr.ensure_instance().await.expect("ensure must succeed");

    let dir = mgr
        .profile_dir()
        .await
        .expect("managed instance must expose its ephemeral profile dir");
    assert!(
        dir.exists(),
        "ephemeral profile dir must exist while the browser is alive: {dir:?}"
    );

    mgr.stop_instance().await.unwrap();

    assert!(
        !dir.exists(),
        "ephemeral profile dir must be removed on stop: {dir:?}"
    );
}

#[tokio::test]
#[ignore = "requires a real Chrome installation"]
async fn given_real_chrome_when_multiple_tabs_opened_then_console_logs_are_isolated() {
    let handler = crate::handler::McpHandler::new_with_params(crate::handler::HandlerParams {
        host: "127.0.0.1".into(),
        port: TEST_PORT_BASE + 6,
        browser: crate::cli::BrowserFlags {
            headless: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let session = handler.session(None).await.expect("default session");
    let _ = session.get_or_connect().await.expect("connect");

    // The TabRegistry assigns opaque IDs; the initial tab may be discovered
    // asynchronously and consume an ID between two opens, so we capture the
    // actual IDs instead of assuming 'tab-1'/'tab-2'.
    let open = |label: &'static str| {
        call(
            &handler,
            "open_tab",
            json!({"url": "about:blank", "label": label}),
        )
    };
    let tab_a = extract_tab_id(&open("tab-a").await);
    let tab_b = extract_tab_id(&open("tab-b").await);
    assert_ne!(
        tab_a, tab_b,
        "tab A and tab B must be registered under distinct IDs"
    );

    let log = json!({"tab_id": tab_a, "expression": "console.error('hello from tab A')"});
    call(&handler, "evaluate_js", log).await;
    // Give the event listener's async threads a brief moment to process the log
    tokio::time::sleep(Duration::from_millis(500)).await;

    let logs = |tab: &String| call(&handler, "get_console_logs", json!({ "tab_id": tab }));
    let logs_a = format!("{:?}", logs(&tab_a).await.content);
    assert!(
        logs_a.contains("hello from tab A"),
        "Tab A must contain its own log"
    );
    let logs_b = format!("{:?}", logs(&tab_b).await.content);
    assert!(
        !logs_b.contains("hello from tab A"),
        "Tab B must not contain tab A's log"
    );

    call(&handler, "close_tab", json!({ "tab_id": tab_a })).await;
    let tab_ids = listed_tab_ids(&handler).await;
    assert!(
        !tab_ids.contains(&tab_a),
        "tab A must be removed from the list"
    );
    assert!(tab_ids.contains(&tab_b), "tab B must remain in the list");

    // Cleanup Chrome
    let _ = session.chrome_manager.lock().await.stop_instance().await;
}

/// The tab ids `list_tabs` reports.
async fn listed_tab_ids(handler: &crate::handler::McpHandler) -> Vec<String> {
    let list = call(handler, "list_tabs", json!({})).await;
    let json: serde_json::Value =
        serde_json::from_str(list.content[0].as_text().expect("text")).expect("JSON");
    json["tabs"]
        .as_array()
        .expect("tabs must be an array")
        .iter()
        .filter_map(|t| t["tab_id"].as_str().map(str::to_string))
        .collect()
}

/// Calls a tool through the handler's table and expects success.
async fn call(
    handler: &crate::handler::McpHandler,
    name: &str,
    arguments: serde_json::Value,
) -> CallToolResult {
    let params: rust_mcp_sdk::schema::CallToolRequestParams =
        serde_json::from_value(json!({"name": name, "arguments": arguments})).unwrap();
    crate::handler::registry::call(handler, params)
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"))
}

/// Extracts the tab_id from an open_tab response body, e.g.
/// `{"tab_id": "tab-2", ...}` → `tab-2`.
fn extract_tab_id(result: &CallToolResult) -> String {
    let text = result.content[0]
        .as_text()
        .expect("open_tab response must be text");
    let json: serde_json::Value =
        serde_json::from_str(text).expect("open_tab response must be JSON");
    json["tab_id"]
        .as_str()
        .expect("open_tab response must contain a tab_id")
        .to_string()
}
