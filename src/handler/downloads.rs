//! The files pages download, the same on both browsers (#77). Each
//! instance saves them in a directory of its own: `--downloads-dir`'s
//! folder named after the instance id when given (see [`instance_folder`]), else a `downloads` folder inside the
//! instance's temporary profile (deleted with it; snap Firefox can only
//! write under its profile's root), else a new folder in the system temp
//! directory (an attached browser, or `--user-profile`). The browser
//! reports each download; an input action that started one says where the
//! file went, and `list_downloads` lists them.

use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Most downloads an instance remembers (the oldest are forgotten; their
/// files stay).
pub(crate) const MAX_DOWNLOADS: usize = 100;

/// Longest wait of an input action for the downloads it started to end,
/// so its reply can name the saved file.
pub(crate) const ACTION_WAIT: Duration = Duration::from_secs(2);

/// How often [`wait_ended`] looks again.
const POLL: Duration = Duration::from_millis(20);

/// Most numbered names tried for a file (`name (1).ext`...) before the
/// browser's own name is kept.
const MAX_NUMBERED: u32 = 100;

/// Where a download stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DownloadStatus {
    InProgress,
    Completed,
    Canceled,
}

/// One download, as the browser reported it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Download {
    /// The browser's id of the download (Chrome's guid, Firefox's id).
    pub(crate) id: String,
    pub(crate) url: String,
    /// The name the page or the server suggested.
    pub(crate) file_name: String,
    /// Where the file is, once complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<PathBuf>,
    pub(crate) state: DownloadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) received_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_bytes: Option<u64>,
    /// The tab whose page started it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tab_id: Option<String>,
}

impl Download {
    /// A download that just began.
    pub(crate) fn begun(id: &str, url: &str, file_name: &str, tab_id: Option<String>) -> Self {
        Self {
            id: id.to_string(),
            url: url.to_string(),
            file_name: file_name.to_string(),
            path: None,
            state: DownloadStatus::InProgress,
            received_bytes: None,
            total_bytes: None,
            tab_id,
        }
    }
}

/// The downloads of one instance and where they are saved.
#[derive(Debug, Default)]
pub(crate) struct DownloadState {
    /// `--downloads-dir/<instance id>`, when given.
    root: Option<PathBuf>,
    /// Where the browser saves, once set up for the running browser.
    dir: Option<PathBuf>,
    list: VecDeque<Download>,
    /// Downloads begun since the browser started (the forgotten included).
    begun: u64,
}

impl DownloadState {
    /// The state of an instance saving in `root` when given.
    pub(crate) fn with_root(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Default::default()
        }
    }

    /// Picks and creates the directory of a browser that started, whose
    /// profile is `profile` (none for an attached browser or the user's
    /// profile); an absolute path, as browsers want.
    pub(crate) fn prepare(&mut self, profile: Option<&Path>) -> std::io::Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        let dir = match (&self.root, profile) {
            (Some(root), _) => root.clone(),
            (None, Some(profile)) => profile.join("downloads"),
            (None, None) => tempfile::Builder::new()
                .prefix("browser-control-mcp-downloads-")
                .tempdir()?
                .keep(),
        };
        std::fs::create_dir_all(&dir)?;
        let dir = std::path::absolute(&dir)?;
        self.dir = Some(dir.clone());
        Ok(dir)
    }

    /// Forgets the downloads and the directory of a browser that stopped
    /// (an ephemeral profile went with its files); the root stays.
    pub(crate) fn reset(&mut self) {
        *self = Self::with_root(self.root.take());
    }

    /// Where the browser saves, once set up.
    pub(crate) fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Records a download that began.
    pub(crate) fn begin(&mut self, download: Download) {
        self.begun += 1;
        self.list.push_back(download);
        if self.list.len() > MAX_DOWNLOADS {
            self.list.pop_front();
        }
    }

    /// The download `id`, to record its progress.
    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut Download> {
        self.list.iter_mut().find(|d| d.id == id)
    }

    /// How many downloads began so far (see [`Self::since`]).
    pub(crate) fn begun(&self) -> u64 {
        self.begun
    }

    /// The downloads that began after [`Self::begun`] was `begun`.
    pub(crate) fn since(&self, begun: u64) -> Vec<Download> {
        let newer = usize::try_from(self.begun.saturating_sub(begun)).unwrap_or(usize::MAX);
        let skip = self.list.len().saturating_sub(newer);
        self.list.iter().skip(skip).cloned().collect()
    }
}

/// The folder of instance `id` in `--downloads-dir` `dir` (a label is the
/// agent's text): the id when it is only lowercase ASCII letters, digits,
/// `-` and `_`. Any other byte is written `~` and two lowercase hex digits
/// (`a/b` is `a~2fb`, `A` is `~41`), and an empty id or a name Windows
/// reserves (`con`, `com1`...) gets a trailing `~` (`instance~` for the
/// empty one). The name only has lowercase characters and spells every
/// byte it replaced, so two ids never share a folder, on case-insensitive
/// file systems either (#113: `a/b` and `a_b` did; #120: a 32-bit hash of
/// the replaced bytes collided).
pub(crate) fn instance_folder(dir: &Path, id: &str) -> PathBuf {
    let safe = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
    let mut name = String::with_capacity(id.len());
    for byte in id.bytes() {
        if safe(byte) {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("~{byte:02x}"));
        }
    }
    // Only an id without escaped bytes can be empty or reserved here; a
    // lone trailing `~` is no escape, so it cannot be another id's name.
    if name.is_empty() {
        name.push_str("instance~");
    } else if is_reserved_on_windows(&name) {
        name.push('~');
    }
    dir.join(name)
}

/// Whether Windows reserves `name` (a device name) in every directory.
fn is_reserved_on_windows(name: &str) -> bool {
    const DEVICES: [&str; 4] = ["con", "prn", "aux", "nul"];
    let numbered = ["com", "lpt"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && n.bytes().all(|b| (b'1'..=b'9').contains(&b)))
    });
    DEVICES.contains(&name) || numbered
}

/// Gives the file Chrome saved as `saved` (named by its guid) the name
/// `name` in `dir`, numbered (`name (1).ext`) when taken; never replaces a
/// file (a hard link to an existing name fails). The new path, or `saved`
/// when it cannot be renamed.
pub(crate) fn place(dir: &Path, saved: &Path, name: &str) -> PathBuf {
    // Only a file name: a suggested name cannot leave the directory.
    let Some(name) = Path::new(name).file_name().map(Path::new) else {
        return saved.to_path_buf();
    };
    for n in 0..MAX_NUMBERED {
        let target = dir.join(numbered(name, n));
        match std::fs::hard_link(saved, &target) {
            Ok(()) => {
                // Best effort: a leftover guid file is only clutter.
                let _ = std::fs::remove_file(saved);
                return target;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    saved.to_path_buf()
}

/// `name`, or `name (n).ext` for `n` above 0.
fn numbered(name: &Path, n: u32) -> std::ffi::OsString {
    if n == 0 {
        return name.as_os_str().to_owned();
    }
    let mut numbered = name.file_stem().unwrap_or_default().to_owned();
    numbered.push(format!(" ({n})"));
    if let Some(extension) = name.extension() {
        numbered.push(".");
        numbered.push(extension);
    }
    numbered
}

/// The sentence an input action's reply adds for the downloads it started.
pub(crate) fn action_note(downloads: &[Download]) -> Option<String> {
    let sentences: Vec<String> = downloads.iter().map(sentence).collect();
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// What became of one download an action started.
fn sentence(download: &Download) -> String {
    let name = &download.file_name;
    match (download.state, &download.path) {
        (DownloadStatus::Completed, Some(path)) => {
            let size = download
                .received_bytes
                .or(download.total_bytes)
                .map(|bytes| format!(" ({bytes} bytes)"))
                .unwrap_or_default();
            format!("The action downloaded {name}{size} to {}.", path.display())
        }
        (DownloadStatus::Canceled, _) => format!(
            "The download of {name} was canceled (by the page, or the browser could not write to the downloads directory)."
        ),
        _ => {
            let progress = match (download.received_bytes, download.total_bytes) {
                (Some(received), Some(total)) => format!(" ({received} of {total} bytes so far)"),
                (Some(received), None) => format!(" ({received} bytes so far)"),
                _ => String::new(),
            };
            format!(
                "The action started downloading {name}{progress}: list_downloads shows where it is saved once complete."
            )
        }
    }
}

/// The downloads begun after `begun`, once none is in progress (at most
/// `limit`).
pub(crate) async fn wait_ended(
    state: &Mutex<DownloadState>,
    begun: u64,
    limit: Duration,
) -> Vec<Download> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let started = state.lock().await.since(begun);
        let busy = started
            .iter()
            .any(|d| d.state == DownloadStatus::InProgress);
        if !busy || tokio::time::Instant::now() >= deadline {
            return started;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[macros::mcp_tool(
    name = "list_downloads",
    description = "Lists the files the instance's pages downloaded, oldest first (id, url, file_name, path once complete, state: in_progress, completed or canceled, sizes, tab_id), and their directory (a folder of --downloads-dir named after the instance id, else the instance's temporary profile, deleted with it). An action that starts a download also says where the file went.",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct ListDownloadsTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
}

impl ListDownloadsTool {
    /// `list_downloads` on a Chrome instance.
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &crate::handler::McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id).await?;
        Ok(listed(&session.downloads).await)
    }
}

/// `list_downloads` on a Firefox instance.
pub(crate) async fn list_firefox(
    params: CallToolRequestParams,
    handler: &crate::handler::McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: ListDownloadsTool = crate::handler::parse_args(params)?;
    let session = handler.firefox(args.instance_id.as_deref()).await?;
    Ok(listed(&session.downloads).await)
}

/// The reply of `list_downloads`.
async fn listed(state: &Arc<Mutex<DownloadState>>) -> CallToolResult {
    let state = state.lock().await;
    let body = json!({"directory": state.dir(), "downloads": state.since(0)});
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    CallToolResult::text_content(vec![text.into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-77: the directory is `--downloads-dir/<instance>` when given,
    /// else the profile's `downloads` folder, else a new temp folder; it is
    /// created, absolute, and forgotten when the browser stops.
    #[test]
    fn the_directory_follows_the_flag_then_the_profile() {
        let base = tempfile::tempdir().unwrap();
        let mut flagged = DownloadState::with_root(Some(base.path().join("flag/e2e")));
        let dir = flagged.prepare(Some(&base.path().join("profile"))).unwrap();
        assert_eq!(dir, base.path().join("flag/e2e"));
        assert!(dir.is_dir());
        let mut profiled = DownloadState::default();
        let dir = profiled
            .prepare(Some(&base.path().join("profile")))
            .unwrap();
        assert_eq!(dir, base.path().join("profile/downloads"));
        assert_eq!(profiled.dir(), Some(dir.as_path()));
        let mut bare = DownloadState::default();
        let dir = bare.prepare(None).unwrap();
        assert!(dir.is_absolute() && dir.is_dir(), "{dir:?}");
        assert!(dir.starts_with(std::env::temp_dir()), "{dir:?}");
        std::fs::remove_dir(&dir).unwrap();
        profiled.begin(Download::begun("g1", "http://a/f", "f", None));
        profiled.reset();
        assert_eq!((profiled.dir(), profiled.begun()), (None, 0));
    }

    /// ISSUE-77: an instance's folder is named after its id, which cannot
    /// leave `--downloads-dir`. #113: two ids never share a folder (`a/b`
    /// and `a_b` did), on case-insensitive file systems either (`A`, `a`),
    /// not even with an escaped one's name; a safe lowercase id keeps its
    /// own name, unless empty or a device name Windows reserves.
    #[test]
    fn instance_folders_stay_inside_the_directory_and_apart() {
        let dir = Path::new("dl");
        let folder = |id: &str| instance_folder(dir, id);
        for id in [
            "default",
            "firefox-2",
            "chrome-1",
            "a_b",
            "a",
            "com",
            "com0",
            "console",
            "instance",
        ] {
            assert_eq!(folder(id), dir.join(id), "{id}");
        }
        for (id, name) in [
            ("../up", "~2e~2e~2fup"),
            ("a/b\\c", "a~2fb~5cc"),
            ("a/b", "a~2fb"),
            ("A", "~41"),
            ("MyLabel", "~4dy~4cabel"),
            ("a~2fb", "a~7e2fb"),
            ("é", "~c3~a9"),
            ("", "instance~"),
            ("con", "con~"),
            ("NUL", "~4e~55~4c"),
            ("lpt9", "lpt9~"),
        ] {
            assert_eq!(folder(id), dir.join(name), "{id:?}");
        }
        // A plain id cannot take an escaped one's folder
        assert_eq!(folder("a_b-3a8e75c1"), dir.join("a_b-3a8e75c1"));
    }

    /// #120: two ids that clean alike and share an FNV-1a hash (`!@^^&!`
    /// and `!$%*!!`) got one folder; the name now spells every byte it
    /// replaces, so no two ids share one.
    #[test]
    fn ids_that_clean_alike_never_share_a_folder() {
        // Given two labels the #113 scheme named `______~c64b3899`
        let dir = Path::new("dl");
        let (first, second) = ("!@^^&!", "!$%*!!");
        // When each gets its folder
        let (a, b) = (instance_folder(dir, first), instance_folder(dir, second));
        // Then the folders differ, also once case-folded
        assert_ne!(a, b);
        let folded = |p: &Path| p.to_string_lossy().to_lowercase();
        assert_ne!(folded(&a), folded(&b));
        assert_eq!(a, dir.join("~21~40~5e~5e~26~21"));
        // And distinct ids of every short shape get distinct folders
        let alphabet = ["a", "A", "_", "-", "~", "/", "~7e", "~41", "con", ""];
        let mut seen = std::collections::HashMap::new();
        for x in alphabet {
            for y in alphabet {
                let id = format!("{x}{y}");
                let name = folded(&instance_folder(dir, &id));
                if let Some(other) = seen.insert(name.clone(), id.clone()) {
                    assert_eq!(other, id, "{other:?} and {id:?} share {name}");
                }
            }
        }
    }

    /// ISSUE-77: the downloads an action started are those begun after it
    /// read the count; the oldest are forgotten past the cap.
    #[test]
    fn downloads_since_a_count() {
        let mut state = DownloadState::default();
        state.begin(Download::begun("g1", "http://a/1", "1.csv", None));
        let before = state.begun();
        state.begin(Download::begun(
            "g2",
            "http://a/2",
            "2.csv",
            Some("tab-1".into()),
        ));
        let ids: Vec<String> = state.since(before).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["g2"]);
        assert_eq!(state.since(0).len(), 2);
        for n in 0..MAX_DOWNLOADS {
            state.begin(Download::begun(&format!("x{n}"), "http://a/x", "x", None));
        }
        assert_eq!(state.since(0).len(), MAX_DOWNLOADS);
        assert_eq!(state.since(before).len(), MAX_DOWNLOADS);
        assert_eq!(state.since(before + 101).len(), 0);
    }

    /// ISSUE-77 (Chrome): a file saved under its guid gets its name, then
    /// `name (1).ext`: Chrome's `allow` replaced a file of the same name.
    #[test]
    fn saved_files_get_their_name_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let save = |guid: &str, body: &str| {
            let saved = dir.path().join(guid);
            std::fs::write(&saved, body).unwrap();
            place(dir.path(), &saved, "data.csv")
        };
        let first = save("guid-1", "one");
        let second = save("guid-2", "two");
        assert_eq!(first, dir.path().join("data.csv"));
        assert_eq!(second, dir.path().join("data (1).csv"));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
        assert!(!dir.path().join("guid-1").exists());
        // A name that tries to leave the directory keeps only its file name
        let escaping = save("guid-3", "three");
        let third = place(dir.path(), &escaping, "../../etc/passwd");
        assert_eq!(third, dir.path().join("passwd"));
        // No usable name: the browser's file stays as it is
        let saved = dir.path().join("guid-4");
        std::fs::write(&saved, "four").unwrap();
        assert_eq!(place(dir.path(), &saved, ".."), saved);
    }

    /// ISSUE-77: an action's reply names each file it downloaded, or says
    /// one is still downloading or was canceled.
    #[test]
    fn action_notes_name_the_files() {
        let mut done = Download::begun("g1", "http://a/d", "data.csv", None);
        done.state = DownloadStatus::Completed;
        done.path = Some(PathBuf::from("/dl/data.csv"));
        done.received_bytes = Some(8);
        let mut busy = Download::begun("g2", "http://a/big", "big.zip", None);
        busy.received_bytes = Some(3);
        busy.total_bytes = Some(10);
        let mut gone = Download::begun("g3", "http://a/x", "x.bin", None);
        gone.state = DownloadStatus::Canceled;
        assert_eq!(action_note(&[]), None);
        let path = Path::new("/dl/data.csv").display();
        assert_eq!(
            action_note(std::slice::from_ref(&done)).unwrap(),
            format!("The action downloaded data.csv (8 bytes) to {path}.")
        );
        let note = action_note(&[done, busy, gone]).unwrap();
        assert!(note.contains("The action started downloading big.zip (3 of 10 bytes so far): list_downloads shows where it is saved once complete."), "{note}");
        assert!(
            note.contains("The download of x.bin was canceled (by the page, or the browser could not write to the downloads directory)."),
            "{note}"
        );
    }

    /// ISSUE-77: the wait returns at once without downloads, and once the
    /// ones begun completed.
    #[tokio::test]
    async fn the_wait_ends_with_the_downloads() {
        let state = Arc::new(Mutex::new(DownloadState::default()));
        let start = std::time::Instant::now();
        assert!(wait_ended(&state, 0, ACTION_WAIT).await.is_empty());
        state
            .lock()
            .await
            .begin(Download::begun("g1", "http://a/d", "d", None));
        let finisher = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            finisher.lock().await.get_mut("g1").unwrap().state = DownloadStatus::Completed;
        });
        let ended = wait_ended(&state, 0, ACTION_WAIT).await;
        assert_eq!(ended[0].state, DownloadStatus::Completed);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
