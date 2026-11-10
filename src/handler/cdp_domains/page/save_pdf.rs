//! `save_pdf`: the shared arguments, path checks and reply, and the Chrome
//! implementation (`Page.printToPDF`).

use crate::handler::McpHandler;
use crate::handler::cdp_domains::input::upload_file::allowed_roots;
use base64::Engine;
use rust_mcp_sdk::{
    macros,
    schema::{
        BlobResourceContents, CallToolError, CallToolRequestParams, CallToolResult, ContentBlock,
        EmbeddedResource,
    },
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[macros::mcp_tool(
    name = "save_pdf",
    description = "Prints the tab to PDF as the browser's print does (print CSS applies). With 'path' (an absolute .pdf under the upload roots: the working and temp directories and --upload-root ones) writes it and returns {path, bytes, pages}; without it, returns the PDF embedded (at most 5 MB)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SavePdfTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Absolute path of the .pdf file to write (overwritten if it exists).
    pub path: Option<String>,
    /// Landscape pages. Default: portrait.
    pub landscape: Option<bool>,
    /// Print background colours and images. Default: false.
    pub print_background: Option<bool>,
    /// Scale, 0.1 to 2. Default: 1.
    pub scale: Option<f64>,
    /// Pages to print, e.g. '1-3, 5'. Default: all.
    pub page_ranges: Option<String>,
}

/// Largest PDF returned in the reply (base64 makes it a third larger).
const MAX_INLINE: usize = 5_000_000;

impl SavePdfTool {
    /// Checks the arguments before any browser work: the file to write, if any.
    pub(crate) fn checked(&self, roots: &[PathBuf]) -> Result<Option<OutputFile>, CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        if let Some(scale) = self.scale
            && !(0.1..=2.0).contains(&scale)
        {
            return fail(format!("'scale' must be 0.1 to 2, got {scale}"));
        }
        if let Some(ranges) = &self.page_ranges
            && (ranges.trim().is_empty() || self.ranges().iter().any(|r| !valid_range(r)))
        {
            return fail(format!(
                "'page_ranges' must look like '1-3, 5' (pages from 1), got '{ranges}'"
            ));
        }
        match &self.path {
            Some(path) => output_path(path, roots, (".pdf", &[".pdf"]))
                .map(Some)
                .map_err(CallToolError::from_message),
            None => Ok(None),
        }
    }

    /// The page ranges, one per comma.
    fn ranges(&self) -> Vec<&str> {
        let ranges = self.page_ranges.as_deref().unwrap_or_default();
        ranges.split(',').map(str::trim).collect()
    }

    /// `Page.printToPDF`'s params.
    fn cdp_params(&self) -> Value {
        let mut params = json!({
            "landscape": self.landscape.unwrap_or(false),
            "printBackground": self.print_background.unwrap_or(false),
            "scale": self.scale.unwrap_or(1.0),
        });
        if self.page_ranges.is_some() {
            params["pageRanges"] = json!(self.ranges().join(","));
        }
        params
    }

    /// `browsingContext.print`'s params for `context`.
    pub(crate) fn bidi_params(&self, context: &str) -> Value {
        let orientation = if self.landscape == Some(true) {
            "landscape"
        } else {
            "portrait"
        };
        let mut params = json!({
            "context": context,
            "orientation": orientation,
            "background": self.print_background.unwrap_or(false),
            "scale": self.scale.unwrap_or(1.0),
        });
        if self.page_ranges.is_some() {
            params["pageRanges"] = json!(self.ranges());
        }
        params
    }

    /// The reply for the PDF the browser printed (`data`, base64): written
    /// to `path`, or embedded in the reply.
    pub(crate) async fn deliver(
        &self,
        data: Option<&str>,
        path: Option<OutputFile>,
    ) -> Result<CallToolResult, CallToolError> {
        let decoded = data.and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok());
        let Some(pdf) = decoded.filter(|bytes| bytes.starts_with(b"%PDF-")) else {
            return Err(CallToolError::from_message(
                "the browser returned no PDF for the tab",
            ));
        };
        let pages = page_count(&pdf);
        let Some(file) = path else {
            return embedded(&pdf, pages);
        };
        let written = write_output(&file, &pdf).await;
        let path = file.path();
        if let Err(e) = written {
            let shown = path.display();
            return Err(CallToolError::from_message(format!(
                "could not write '{shown}': {e}"
            )));
        }
        let shown = path.display().to_string();
        let first = format!(
            "Saved a {}-byte PDF{} to {shown}.",
            pdf.len(),
            pages
                .map(|n| format!(" of {n} page(s)"))
                .unwrap_or_default()
        );
        let body = json!({"path": shown, "bytes": pdf.len(), "pages": pages});
        let body = serde_json::to_string_pretty(&body).unwrap_or_default();
        Ok(CallToolResult::text_content(vec![
            format!("{first}\n{body}").into(),
        ]))
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: SavePdfTool = crate::handler::parse_args(params)?;
        let path = args.checked(&handler.upload_roots)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let method = "Page.printToPDF";
        let reply =
            crate::handler::send_or(&target, method, args.cdp_params(), "CDP Error").await?;
        let result = reply.result.unwrap_or_default();
        args.deliver(result["data"].as_str(), path).await
    }
}

/// `1`, `2-4`, `-3` or `5-`: what both browsers take.
fn valid_range(range: &str) -> bool {
    let page = |s: &str| s.is_empty() || s.parse::<u32>().is_ok_and(|n| n > 0);
    match range.split_once('-') {
        Some((from, to)) => page(from.trim()) && page(to.trim()) && range != "-",
        None => !range.is_empty() && page(range),
    }
}

/// A file to write that [`output_path`] checked: `relative`, a path below
/// `root`, the canonical upload root it is under. [`write_output`] reaches
/// it from `root` again, never by its whole path (#115).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputFile {
    root: PathBuf,
    relative: PathBuf,
}

impl OutputFile {
    /// The file `name` in `directory`, a directory of the server's own
    /// (`record_video`'s temporary file in the system temp directory).
    pub(crate) fn in_directory(directory: &Path, name: &std::ffi::OsStr) -> Self {
        Self {
            root: directory.to_path_buf(),
            relative: PathBuf::from(name),
        }
    }

    /// The file's whole path, for replies.
    pub(crate) fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }
}

/// The file `path` names, once checked: absolute, ending in one of the
/// `endings` (case-insensitive; `kind` names them in the error), in an
/// existing directory under one of `roots` (default: the working and temp
/// directories) once resolved, so `..` and symlinks cannot leave them.
pub(crate) fn output_path(
    path: &str,
    roots: &[PathBuf],
    (kind, endings): (&str, &[&str]),
) -> Result<OutputFile, String> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return Err(format!("'{path}' is not an absolute path"));
    }
    let lower = path.to_ascii_lowercase();
    let fits = endings.iter().any(|ending| lower.ends_with(ending));
    let (Some(directory), Some(name), true) = (given.parent(), given.file_name(), fits) else {
        return Err(format!("'{path}' must name a {kind} file"));
    };
    let Ok(directory) = std::fs::canonicalize(directory) else {
        return Err(format!("the directory of '{path}' does not exist"));
    };
    let file = directory.join(name);
    if file.is_dir() {
        return Err(format!("'{path}' is a directory"));
    }
    // An existing file (or symlink) must resolve under the roots too. A
    // dangling symlink is refused: writing through it would create its
    // target wherever it points, outside the roots too (#106).
    let is_link = std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_symlink());
    let resolved = match std::fs::canonicalize(&file) {
        Ok(resolved) => resolved,
        Err(_) if is_link => {
            return Err(format!(
                "'{path}' is a symlink whose target does not exist; name a regular file under the upload roots"
            ));
        }
        Err(_) => file,
    };
    let roots = allowed_roots(roots);
    let under = roots.iter().find_map(|root| {
        let relative = resolved.strip_prefix(root).ok()?;
        Some(OutputFile {
            root: root.clone(),
            relative: relative.to_path_buf(),
        })
    });
    under.ok_or_else(|| {
        let shown: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        format!(
            "'{path}' is outside the upload roots ({}); start the server with --upload-root <dir> to allow another directory",
            shown.join(", ")
        )
    })
}

/// Writes `bytes` to `file`: from a handle on its upload root, it opens the
/// file's directory without leaving the root (a directory swapped for a
/// symlink to outside after the check fails the write, #115), creates a new
/// file there and moves it into place, so a symlink planted at the file
/// itself is replaced, never followed (#106). A replaced file keeps its
/// mode; a failed write leaves no file behind.
pub(crate) async fn write_output(file: &OutputFile, bytes: &[u8]) -> std::io::Result<()> {
    let file = file.clone();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || write_beneath(&file, &bytes))
        .await
        .map_err(std::io::Error::other)?
}

/// [`write_output`]'s blocking work.
fn write_beneath(file: &OutputFile, bytes: &[u8]) -> std::io::Result<()> {
    use cap_std::fs::Dir;
    let root = Dir::open_ambient_dir(&file.root, cap_std::ambient_authority())?;
    let Some(name) = file.relative.file_name() else {
        return Err(std::io::Error::other("the path names no file"));
    };
    let directory = match file.relative.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => root.open_dir(parent).map_err(|e| {
            // cap-std's own words ("a path led outside of the filesystem").
            let left = e.kind() == std::io::ErrorKind::PermissionDenied
                && e.to_string().contains("outside of the filesystem");
            match left {
                true => std::io::Error::other(format!(
                    "its directory now leads outside the upload root {} (was it replaced by a symlink?)",
                    file.root.display()
                )),
                false => e,
            }
        })?,
        _ => root,
    };
    let replaced = directory
        .symlink_metadata(name)
        .ok()
        .filter(cap_std::fs::Metadata::is_file)
        .map(|meta| meta.permissions());
    let (mut temporary, temporary_name) = new_file(&directory)?;
    // The mode first: a private file's bytes never sit in a readable one.
    let written = match replaced {
        Some(permissions) => temporary.set_permissions(permissions),
        None => Ok(()),
    }
    .and_then(|()| std::io::Write::write_all(&mut temporary, bytes))
    .and_then(|()| directory.rename(&temporary_name, &directory, name));
    if written.is_err() {
        let _ = directory.remove_file(&temporary_name);
    }
    written
}

/// A new file of `directory`, named `.browser-control-mcp-<pid>-<n>`, with
/// the mode `std::fs::write` gives a new file (the umask applies).
fn new_file(directory: &cap_std::fs::Dir) -> std::io::Result<(cap_std::fs::File, String)> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    for _ in 0..100 {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!(".browser-control-mcp-{}-{n}", std::process::id());
        match directory.open_with(&name, &options) {
            Ok(file) => return Ok((file, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for a new file"))
}

/// The PDF's page objects (`/Type /Page`, not `/Pages`), when it has any
/// uncompressed: both browsers write them so.
fn page_count(pdf: &[u8]) -> Option<usize> {
    let marker = b"/Type /Page";
    let pages = pdf
        .windows(marker.len() + 1)
        .filter(|w| w.starts_with(marker) && w[marker.len()] != b's')
        .count();
    (pages > 0).then_some(pages)
}

/// The PDF as an embedded resource, with a line saying what it is.
fn embedded(pdf: &[u8], pages: Option<usize>) -> Result<CallToolResult, CallToolError> {
    if pdf.len() > MAX_INLINE {
        return Err(CallToolError::from_message(format!(
            "the PDF has {} bytes, over the {MAX_INLINE} a reply carries: pass a 'path' to write it to a file",
            pdf.len()
        )));
    }
    let blob = base64::engine::general_purpose::STANDARD.encode(pdf);
    let uri = "browser-control://save_pdf/page.pdf".to_string();
    let contents = BlobResourceContents::new(blob, uri).with_mime_type("application/pdf");
    let resource = EmbeddedResource::new(contents.into(), None, None);
    let first = format!(
        "A {}-byte PDF{}.",
        pdf.len(),
        pages
            .map(|n| format!(" of {n} page(s)"))
            .unwrap_or_default()
    );
    Ok(CallToolResult {
        content: vec![
            ContentBlock::TextContent(first.into()),
            ContentBlock::EmbeddedResource(resource),
        ],
        is_error: None,
        meta: None,
        structured_content: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};

    /// A two-page PDF as both browsers write the page objects.
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj <</Type /Pages /Count 2>>\n2 0 obj <</Type /Page>>\n3 0 obj <</Type /Page>>\n%%EOF";

    fn tool(args: Value) -> SavePdfTool {
        serde_json::from_value(args).unwrap()
    }

    /// PDF-01: the options and the path are checked before any browser
    /// work; the path must be an absolute .pdf under the upload roots.
    #[test]
    fn arguments_and_paths_are_checked() {
        let root = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        let fails = |args: Value| tool(args).checked(&roots).unwrap_err().to_string();
        assert!(fails(json!({"scale": 3.0})).contains("0.1 to 2"));
        assert!(fails(json!({"page_ranges": "1-x"})).contains("'1-3, 5'"));
        assert!(fails(json!({"page_ranges": "0"})).contains("pages from 1"));
        assert!(fails(json!({"path": "out.pdf"})).contains("absolute"));
        let inside = |name: &str| root.path().join(name).display().to_string();
        assert!(fails(json!({"path": inside("out.txt")})).contains(".pdf"));
        assert!(fails(json!({"path": inside("missing/out.pdf")})).contains("does not exist"));
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("out.pdf").display().to_string();
        assert!(fails(json!({"path": elsewhere})).contains("upload roots"));
        // A valid call
        let ok = tool(json!({"path": inside("out.PDF"), "page_ranges": "1-3, 5, 7-"}));
        assert!(ok.checked(&roots).unwrap().is_some());
        assert!(tool(json!({})).checked(&roots).unwrap().is_none());
    }

    /// PDF-02: the BiDi params name the orientation and the ranges as a
    /// list; Chrome's take the ranges as one string.
    #[test]
    fn options_map_to_each_protocol() {
        let args = tool(
            json!({"landscape": true, "print_background": true, "scale": 0.5,
            "page_ranges": "1-2, 4"}),
        );
        assert_eq!(
            args.cdp_params(),
            json!({"landscape": true, "printBackground": true, "scale": 0.5, "pageRanges": "1-2,4"})
        );
        assert_eq!(
            args.bidi_params("ctx"),
            json!({"context": "ctx", "orientation": "landscape", "background": true, "scale": 0.5,
                "pageRanges": ["1-2", "4"]})
        );
        let defaults = tool(json!({}));
        assert_eq!(
            defaults.bidi_params("ctx"),
            json!({"context": "ctx", "orientation": "portrait", "background": false, "scale": 1.0})
        );
    }

    /// PDF-03 (Chrome): the printed PDF is written to the path, and the
    /// reply says where, its size and pages.
    #[tokio::test]
    async fn chrome_writes_the_pdf() {
        // Given a tab Chrome prints to a two-page PDF
        let mock = spawn_recording_mock().await;
        let data = base64::engine::general_purpose::STANDARD.encode(PDF);
        mock.respond_with(
            "Page.printToPDF",
            move |_| json!({"result": {"data": data.clone()}}),
        );
        let mut handler = mock_handler(mock.http_port);
        let root = tempfile::tempdir().unwrap();
        handler.upload_roots = vec![root.path().to_path_buf()];
        let path = root.path().join("page.pdf");
        // When save_pdf runs with a path
        let args = json!({"path": path.display().to_string(), "landscape": true});
        let reply = SavePdfTool::handle(call("save_pdf", args), &handler)
            .await
            .unwrap();
        // Then the file holds the PDF and the reply describes it
        assert_eq!(std::fs::read(&path).unwrap(), PDF);
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.contains("of 2 page(s)") && text.contains("\"pages\": 2"),
            "{text}"
        );
        let sent = &mock.commands_named("Page.printToPDF")[0].params;
        assert_eq!(sent["landscape"], true);
    }

    /// The names in `dir`, sorted.
    #[cfg(unix)]
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// PDF-05 (#106): a path that is a symlink is followed only to an
    /// existing file under the roots; a dangling one is refused, since the
    /// write would create its target wherever it points.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_output_must_resolve_under_the_roots() {
        use std::os::unix::fs::symlink;
        // Given links in the root: dangling to outside, to an existing file
        // outside, and to an existing file inside
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        std::fs::write(outside.path().join("there.pdf"), "x").unwrap();
        std::fs::write(root.path().join("real.pdf"), "x").unwrap();
        symlink(
            outside.path().join("escaped.pdf"),
            root.path().join("dangling.pdf"),
        )
        .unwrap();
        symlink(
            outside.path().join("there.pdf"),
            root.path().join("out.pdf"),
        )
        .unwrap();
        symlink(root.path().join("real.pdf"), root.path().join("in.pdf")).unwrap();
        let check = |name: &str| {
            let path = root.path().join(name).display().to_string();
            tool(json!({ "path": path })).checked(&roots)
        };
        // When each is checked
        let dangling = check("dangling.pdf").unwrap_err().to_string();
        let out = check("out.pdf").unwrap_err().to_string();
        let inside = check("in.pdf").unwrap();
        // Then only the link to a file inside passes, as the file it names
        assert!(
            dangling.contains("symlink whose target does not exist"),
            "{dangling}"
        );
        assert!(out.contains("outside the upload roots"), "{out}");
        let real = std::fs::canonicalize(root.path().join("real.pdf")).unwrap();
        assert_eq!(inside.map(|file| file.path()), Some(real));
        assert_eq!(names(outside.path()), ["there.pdf"]);
    }

    /// PDF-06 (#106): the file is written by moving a new file into place,
    /// so a symlink created after the check is replaced, never followed,
    /// and no partial file is left behind.
    #[cfg(unix)]
    #[tokio::test]
    async fn writing_replaces_a_symlink_planted_after_the_check() {
        // Given a checked path in the root, then a link planted there
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        let path = root.path().join("late.pdf").display().to_string();
        let checked = tool(json!({ "path": path }))
            .checked(&roots)
            .unwrap()
            .unwrap();
        let escaped = outside.path().join("escaped.pdf");
        std::os::unix::fs::symlink(&escaped, checked.path()).unwrap();
        // When the PDF is written
        write_output(&checked, PDF).await.unwrap();
        let checked = checked.path();
        // Then the path holds the PDF as a regular file and nothing escaped
        assert!(!std::fs::symlink_metadata(&checked).unwrap().is_symlink());
        assert_eq!(std::fs::read(&checked).unwrap(), PDF);
        assert!(!escaped.exists());
        assert_eq!(names(root.path()), ["late.pdf"]);
        // And it has the mode std::fs::write gives a new file
        use std::os::unix::fs::PermissionsExt;
        let reference = outside.path().join("reference");
        std::fs::write(&reference, "x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode(&checked), mode(&reference));
        // And overwriting an existing private file keeps it private
        let private = root.path().join("private.pdf");
        std::fs::write(&private, "x").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = private.display().to_string();
        let checked = tool(json!({ "path": path }))
            .checked(&roots)
            .unwrap()
            .unwrap();
        write_output(&checked, PDF).await.unwrap();
        assert_eq!(mode(&private) & 0o777, 0o600);
        assert_eq!(std::fs::read(&private).unwrap(), PDF);
    }

    /// PDF-07 (#115): the file is created and moved into place inside the
    /// upload root it was checked under, resolved again from that root: a
    /// directory swapped for a symlink to outside after the check fails the
    /// write, and nothing (not even the temporary file) lands outside.
    #[cfg(unix)]
    #[tokio::test]
    async fn writing_never_follows_a_directory_swapped_after_the_check() {
        // Given a checked path in a subdirectory of the root
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let path = sub.join("race.pdf").display().to_string();
        let checked = tool(json!({ "path": path }))
            .checked(&roots)
            .unwrap()
            .unwrap();
        // When the subdirectory is swapped for a link to outside, then the
        // PDF is written
        std::fs::rename(&sub, root.path().join("sub.real")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();
        let written = write_output(&checked, PDF).await;
        // Then the write fails, saying why, and nothing is left anywhere
        let error = written.unwrap_err().to_string();
        assert!(error.contains("leads outside the upload root"), "{error}");
        assert!(
            names(outside.path()).is_empty(),
            "{:?}",
            names(outside.path())
        );
        assert!(names(&root.path().join("sub.real")).is_empty());
        // And once the directory is back, the same file is written in it
        std::fs::remove_file(&sub).unwrap();
        std::fs::rename(root.path().join("sub.real"), &sub).unwrap();
        write_output(&checked, PDF).await.unwrap();
        assert_eq!(std::fs::read(sub.join("race.pdf")).unwrap(), PDF);
        assert_eq!(names(&sub), ["race.pdf"]);
    }

    /// PDF-04: without a path the PDF comes back as an embedded resource;
    /// data that is not a PDF fails.
    #[tokio::test]
    async fn pdf_without_path_is_embedded() {
        let args = tool(json!({}));
        let data = base64::engine::general_purpose::STANDARD.encode(PDF);
        let reply = args.deliver(Some(&data), None).await.unwrap();
        let ContentBlock::EmbeddedResource(resource) = &reply.content[1] else {
            panic!("no embedded resource: {reply:?}");
        };
        let resource = serde_json::to_value(&resource.resource).unwrap();
        assert_eq!(resource["mimeType"], "application/pdf");
        assert_eq!(resource["blob"], data);
        let html = base64::engine::general_purpose::STANDARD.encode("<html>");
        let error = args.deliver(Some(&html), None).await.unwrap_err();
        assert!(error.to_string().contains("no PDF"), "{error}");
    }
}
