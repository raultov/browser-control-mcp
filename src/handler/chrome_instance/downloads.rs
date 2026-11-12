//! Chrome's downloads (#77): the browser-level connection asks Chrome to
//! save every download in the instance's directory and report it
//! (`Browser.setDownloadBehavior` with `allowAndName`: Chrome's `allow`
//! replaces a file of the same name, so each is saved under its guid and
//! then given its name without overwriting, see `downloads::place`), then
//! records `Browser.downloadWillBegin` / `Browser.downloadProgress`.

use crate::handler::chrome_instance::tab_registry::TabRegistry;
use crate::handler::downloads::{Download, DownloadState, DownloadStatus};
use crate::util::json::str_field;
use cdp_browser_lite::{BrowserClient, WsResponse};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

/// Sets up the downloads of the browser `browser` connects to, whose
/// profile is `profile`, and starts recording them; `None` when Chrome
/// refused (the downloads then go where Chrome puts them, unreported), or
/// when the directory could not be created: then Chrome is told to deny
/// every download, which never lands in its default folder, outside the
/// directory the operator chose (#125).
pub(crate) async fn start_download_listener(
    browser: &BrowserClient,
    state: Arc<Mutex<DownloadState>>,
    tabs: Arc<RwLock<TabRegistry>>,
    profile: Option<&Path>,
) -> Option<tokio::task::JoinHandle<()>> {
    let dir = match state.lock().await.prepare(profile) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(error = %e.kind(), "could not create the downloads directory: downloads are denied");
            let deny = json!({"behavior": "deny"});
            let _ = browser
                .client()
                .send_raw_command("Browser.setDownloadBehavior", deny)
                .await;
            return None;
        }
    };
    // Subscribed first: a download may begin right after.
    let mut events = browser.client().on_domain("Browser");
    let behavior = json!({"behavior": "allowAndName", "downloadPath": dir, "eventsEnabled": true});
    let set = browser
        .client()
        .send_raw_command("Browser.setDownloadBehavior", behavior)
        .await;
    if set.is_err() {
        tracing::warn!("Chrome refused Browser.setDownloadBehavior: downloads are not reported");
        return None;
    }
    Some(tokio::spawn(async move {
        while let Some(event) = events.next().await {
            // The browser-level session's own events only.
            if let Ok(event) = event
                && event.session_id.is_none()
            {
                on_download_event(&event, &state, &tabs).await;
            }
        }
    }))
}

/// Records one `Browser.download*` event.
async fn on_download_event(
    event: &WsResponse,
    state: &Mutex<DownloadState>,
    tabs: &RwLock<TabRegistry>,
) {
    let (Some(method), Some(params)) = (event.method.as_deref(), &event.params) else {
        return;
    };
    let Some(guid) = str_field(params, "guid") else {
        return;
    };
    match method {
        "Browser.downloadWillBegin" => {
            let tab = tab_of(tabs, str_field(params, "frameId").unwrap_or_default());
            let url = str_field(params, "url").unwrap_or_default();
            let name = str_field(params, "suggestedFilename").unwrap_or(guid);
            state
                .lock()
                .await
                .begin(Download::begun(guid, url, name, tab));
        }
        "Browser.downloadProgress" => on_progress(guid, params, state).await,
        _ => {}
    }
}

/// Records a download's progress; once complete, gives its file its name.
async fn on_progress(guid: &str, params: &Value, state: &Mutex<DownloadState>) {
    let bytes = |key: &str| params.get(key).and_then(Value::as_u64);
    let placing = {
        let mut state = state.lock().await;
        let dir = state.dir().map(Path::to_path_buf);
        let Some(download) = state.get_mut(guid) else {
            return;
        };
        download.received_bytes = bytes("receivedBytes").or(download.received_bytes);
        download.total_bytes = bytes("totalBytes")
            .filter(|total| *total > 0)
            .or(download.total_bytes);
        match str_field(params, "state") {
            Some("canceled") => {
                download.state = DownloadStatus::Canceled;
                None
            }
            Some("completed") => {
                let saved = str_field(params, "filePath").map(PathBuf::from);
                saved
                    .zip(dir)
                    .map(|(saved, dir)| (saved, dir, download.file_name.clone()))
            }
            _ => None,
        }
    };
    let Some((saved, dir, name)) = placing else {
        return;
    };
    let placed =
        tokio::task::spawn_blocking(move || crate::handler::downloads::place(&dir, &saved, &name))
            .await;
    if let Some(download) = state.lock().await.get_mut(guid)
        && let Ok(path) = placed
    {
        download.path = Some(path);
        download.state = DownloadStatus::Completed;
    }
}

/// The MCP id of the tab whose main frame is `frame` (Chrome's main frame
/// id is its target id).
fn tab_of(tabs: &RwLock<TabRegistry>, frame: &str) -> Option<String> {
    let registry = crate::util::sync::read(tabs);
    registry
        .tabs
        .iter()
        .find(|(_, entry)| entry.tab.target_id() == frame)
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::spawn_recording_mock;

    /// Waits (at most 2 s) until `done`.
    async fn eventually(mut done: impl FnMut() -> bool) {
        for _ in 0..100 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// ISSUE-77 (Chrome): downloads are saved in the instance's directory
    /// under their guid, reported on the browser-level session, then given
    /// their name; a tab session's copy of the events is ignored.
    #[tokio::test]
    async fn downloads_are_saved_and_named() {
        // Given a browser-level connection and a profile
        let mock = spawn_recording_mock().await;
        let browser = BrowserClient::connect(
            &format!("127.0.0.1:{}", mock.http_port),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let profile = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(DownloadState::default()));
        let tabs = Arc::new(RwLock::new(TabRegistry::new(16)));
        // When the listener starts
        let listener = start_download_listener(&browser, state.clone(), tabs, Some(profile.path()));
        let _listener = listener.await.unwrap();
        // Then Chrome saves in the profile's downloads folder, reporting them
        let dir = profile.path().join("downloads");
        let set = mock.commands_named("Browser.setDownloadBehavior");
        let expected =
            json!({"behavior": "allowAndName", "downloadPath": dir, "eventsEnabled": true});
        assert_eq!(
            (set[0].params.clone(), set[0].session_id.clone()),
            (expected, None)
        );
        // And when a download begins and completes
        let begin = json!({"frameId": "F1", "guid": "g1", "url": "http://a/download", "suggestedFilename": "data.csv"});
        mock.emit(
            "Browser.downloadWillBegin",
            begin.clone(),
            Some("TAB-SESSION"),
        );
        mock.emit("Browser.downloadWillBegin", begin, None);
        std::fs::write(dir.join("g1"), "a,b\n").unwrap();
        let done = json!({"guid": "g1", "state": "completed", "receivedBytes": 4, "totalBytes": 4, "filePath": dir.join("g1")});
        mock.emit("Browser.downloadProgress", done, None);
        // Then it is recorded once, its file named data.csv
        let completed = || {
            state.try_lock().is_ok_and(|s| {
                s.since(0)
                    .first()
                    .is_some_and(|d| d.state == DownloadStatus::Completed)
            })
        };
        eventually(completed).await;
        let downloads = state.lock().await.since(0);
        assert_eq!(downloads.len(), 1, "{downloads:?}");
        assert_eq!(
            downloads[0].path.as_deref(),
            Some(dir.join("data.csv").as_path())
        );
        assert_eq!(downloads[0].received_bytes, Some(4));
        assert_eq!(
            std::fs::read_to_string(dir.join("data.csv")).unwrap(),
            "a,b\n"
        );
    }

    /// #125 (Chrome): a downloads directory that cannot be created denies
    /// every download, so none lands in Chrome's default folder (outside
    /// the directory the operator chose).
    #[tokio::test]
    async fn an_uncreatable_directory_denies_downloads() {
        // Given a downloads root under a file, where no folder can be made
        let mock = spawn_recording_mock().await;
        let address = format!("127.0.0.1:{}", mock.http_port);
        let browser = BrowserClient::connect(&address, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        let root = Some(file.path().join("instance"));
        let state = Arc::new(Mutex::new(DownloadState::with_root(root)));
        let tabs = Arc::new(RwLock::new(TabRegistry::new(16)));
        // When the listener starts
        let listener = start_download_listener(&browser, state, tabs, None).await;
        // Then Chrome is told to deny downloads, and nothing records them
        assert!(listener.is_none());
        let set = mock.commands_named("Browser.setDownloadBehavior");
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].params, json!({"behavior": "deny"}));
    }

    /// ISSUE-77 (Chrome): a canceled download says so.
    #[tokio::test]
    async fn canceled_downloads_say_so() {
        let state = Mutex::new(DownloadState::default());
        let tabs = RwLock::new(TabRegistry::new(16));
        let event = |method: &str, params: Value| WsResponse {
            method: Some(method.to_string()),
            params: Some(params),
            ..Default::default()
        };
        let begin = json!({"frameId": "F1", "guid": "g2", "url": "http://a/x", "suggestedFilename": "x.bin"});
        on_download_event(&event("Browser.downloadWillBegin", begin), &state, &tabs).await;
        let gone = json!({"guid": "g2", "state": "canceled", "receivedBytes": 1, "totalBytes": 9});
        on_download_event(&event("Browser.downloadProgress", gone), &state, &tabs).await;
        let download = state.lock().await.since(0)[0].clone();
        assert_eq!(
            (download.state, download.total_bytes),
            (DownloadStatus::Canceled, Some(9))
        );
    }
}
