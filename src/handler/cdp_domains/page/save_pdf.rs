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
    description = "Prints the tab to PDF as the browser's print does (print CSS applies). With 'path', writes the file there (an absolute .pdf path under the upload roots: the server's working and temp directories, or its --upload-root directories) and returns {path, bytes, pages}; without it, returns the PDF as an embedded resource (at most 5 MB)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct SavePdfTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
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
    pub(crate) fn checked(&self, roots: &[PathBuf]) -> Result<Option<PathBuf>, CallToolError> {
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
            Some(path) => output_path(path, roots)
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
        path: Option<PathBuf>,
    ) -> Result<CallToolResult, CallToolError> {
        let decoded = data.and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok());
        let Some(pdf) = decoded.filter(|bytes| bytes.starts_with(b"%PDF-")) else {
            return Err(CallToolError::from_message(
                "the browser returned no PDF for the tab",
            ));
        };
        let pages = page_count(&pdf);
        let Some(path) = path else {
            return embedded(&pdf, pages);
        };
        let written = tokio::fs::write(&path, &pdf).await;
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

/// The file `path` names, once checked: absolute, ending in `.pdf`, in an
/// existing directory under one of `roots` (default: the working and temp
/// directories) once resolved, so `..` and symlinks cannot leave them.
fn output_path(path: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return Err(format!("'{path}' is not an absolute path"));
    }
    let is_pdf = given
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    let (Some(directory), Some(name), true) = (given.parent(), given.file_name(), is_pdf) else {
        return Err(format!("'{path}' must name a .pdf file"));
    };
    let Ok(directory) = std::fs::canonicalize(directory) else {
        return Err(format!("the directory of '{path}' does not exist"));
    };
    let file = directory.join(name);
    if file.is_dir() {
        return Err(format!("'{path}' is a directory"));
    }
    // An existing file (or symlink) must resolve under the roots too.
    let resolved = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
    let roots = allowed_roots(roots);
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        let shown: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        return Err(format!(
            "'{path}' is outside the upload roots ({}); start the server with --upload-root <dir> to allow another directory",
            shown.join(", ")
        ));
    }
    Ok(file)
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
