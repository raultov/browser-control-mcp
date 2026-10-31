use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[macros::mcp_tool(
    name = "upload_file",
    description = "Sets the files of an <input type=file> as if picked in the file chooser (the page gets input and change). Paths must be absolute files under the upload roots: the server's working and temp directories, or its --upload-root directories."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct UploadFileTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Element: CSS selector, 'host >>> inner' (inside an open shadow root or same-origin iframe), or a snapshot ref (e12).
    pub selector: String,
    /// 1 to 20 absolute paths of existing files (several need a 'multiple' input).
    pub paths: Vec<String>,
}

/// Most files one call sets.
const MAX_FILES: usize = 20;

impl UploadFileTool {
    /// The paths to hand the browser, once each was checked: absolute, an
    /// existing file, under one of `roots` (default: the working and temp
    /// directories) once resolved, so `..` and symlinks cannot leave them.
    pub(crate) fn checked_paths(&self, roots: &[PathBuf]) -> Result<Vec<String>, CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        if !(1..=MAX_FILES).contains(&self.paths.len()) {
            return fail(format!(
                "upload_file takes 1 to {MAX_FILES} paths, got {}",
                self.paths.len()
            ));
        }
        let roots = allowed_roots(roots);
        for path in &self.paths {
            if !Path::new(path).is_absolute() {
                return fail(format!("'{path}' is not an absolute path"));
            }
            let Ok(resolved) = std::fs::canonicalize(path) else {
                return fail(format!("'{path}' does not exist"));
            };
            if !resolved.is_file() {
                return fail(format!("'{path}' is not a file"));
            }
            if !roots.iter().any(|root| resolved.starts_with(root)) {
                let shown: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
                return fail(format!(
                    "'{path}' is outside the upload roots ({}); start the server with --upload-root <dir> to allow another directory",
                    shown.join(", ")
                ));
            }
        }
        Ok(self.paths.clone())
    }

    /// Whether the input can take the files (shared by every browser).
    pub(crate) fn check_input(&self, outcome: &Value) -> Result<(), CallToolError> {
        let selector = &self.selector;
        let fail = |message: String| Err(CallToolError::from_message(message));
        match outcome["status"].as_str() {
            Some("ok") if self.paths.len() > 1 && outcome["multiple"] != true => fail(format!(
                "'{selector}' takes one file (no 'multiple' attribute); {} were given",
                self.paths.len()
            )),
            Some("ok") => Ok(()),
            Some("not_file") => fail(format!("'{selector}' is not an <input type=file>")),
            Some("disabled") => fail(format!("'{selector}' is disabled")),
            _ => fail(format!(
                "Element not found for selector: {selector}{}",
                crate::util::js::ref_hint(selector)
            )),
        }
    }

    /// The reply from the names the input holds (shared by every browser).
    pub(crate) fn reply(&self, names: &Value) -> CallToolResult {
        let names: Vec<&str> = names
            .as_array()
            .map(|n| n.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let body = serde_json::to_string_pretty(&json!({ "files": names })).unwrap_or_default();
        let first = format!(
            "Set {} file(s) on '{}': {}.",
            names.len(),
            self.selector,
            names.join(", ")
        );
        CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: UploadFileTool = crate::handler::parse_args(params)?;
        let files = args.checked_paths(&handler.upload_roots)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let selector = &args.selector;
        let input = crate::util::js::file_input(selector);
        args.check_input(&crate::handler::snippet_value(&target, &input).await?)?;
        set_files(&target, selector, files).await?;
        let names = crate::util::js::file_names(selector);
        Ok(args.reply(&crate::handler::snippet_value(&target, &names).await?))
    }
}

/// The roots files may come from, resolved (those that do not exist left out).
pub(crate) fn allowed_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let defaults;
    let roots = if roots.is_empty() {
        defaults = [
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ];
        &defaults[..]
    } else {
        roots
    };
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}

/// `DOM.setFileInputFiles` on the element `selector` names (by its remote
/// object, released afterwards).
async fn set_files(
    target: &CdpTarget,
    selector: &str,
    files: Vec<String>,
) -> Result<(), CallToolError> {
    let params = json!({"expression": crate::util::js::element(selector)});
    let found = crate::handler::send_or(target, "Runtime.evaluate", params, "CDP Error").await?;
    let object = found
        .result
        .as_ref()
        .and_then(|r| r.pointer("/result/objectId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CallToolError::from_message("The file input disappeared."))?;
    let set = json!({"files": files, "objectId": object});
    // Kept as text across the release: a tool error is not `Send`.
    let outcome = target
        .send_raw_command("DOM.setFileInputFiles", set)
        .await
        .map(|_| ())
        .map_err(|e| crate::handler::cdp_failure("CDP Error", &e));
    let release = json!({"objectId": object});
    let _ = target
        .send_raw_command("Runtime.releaseObject", release)
        .await;
    outcome.map_err(CallToolError::from_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An existing file inside the repository (the working directory of
    /// `cargo test`).
    fn manifest() -> String {
        format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"))
    }

    fn tool(paths: &[&str]) -> UploadFileTool {
        serde_json::from_value(json!({"selector": "#up", "paths": paths})).unwrap()
    }

    /// ISSUE-6: paths are checked before anything reaches the browser:
    /// absolute, existing files, under the upload roots (`..` resolved).
    #[test]
    fn paths_are_checked_against_the_roots() {
        let file = manifest();
        assert_eq!(
            tool(&[&file]).checked_paths(&[]).unwrap(),
            vec![file.clone()]
        );
        let error = |paths: &[&str], roots: &[PathBuf]| {
            tool(paths).checked_paths(roots).unwrap_err().to_string()
        };
        assert!(error(&["Cargo.toml"], &[]).contains("not an absolute path"));
        let missing = format!("{}/no-such-file", env!("CARGO_MANIFEST_DIR"));
        assert!(error(&[&missing], &[]).contains("does not exist"));
        assert!(error(&[env!("CARGO_MANIFEST_DIR")], &[]).contains("is not a file"));
        let src = PathBuf::from(format!("{}/src", env!("CARGO_MANIFEST_DIR")));
        let escaping = format!("{}/src/../Cargo.toml", env!("CARGO_MANIFEST_DIR"));
        assert!(error(&[&escaping], &[src]).contains("is outside the upload roots"));
        assert!(error(&[], &[]).contains("1 to 20 paths, got 0"));
    }

    /// ISSUE-6: the input must be an enabled file input, 'multiple' for
    /// several files.
    #[test]
    fn input_must_take_the_files() {
        let one = tool(&["/a"]);
        let two = tool(&["/a", "/b"]);
        assert!(
            one.check_input(&json!({"status": "ok", "multiple": false}))
                .is_ok()
        );
        let error = |t: &UploadFileTool, v: Value| t.check_input(&v).unwrap_err().to_string();
        assert!(error(&two, json!({"status": "ok", "multiple": false})).contains("takes one file"));
        assert!(error(&one, json!({"status": "not_file"})).contains("is not an <input type=file>"));
        assert!(error(&one, json!({"status": "missing"})).starts_with("Element not found"));
    }

    /// ISSUE-6 (Chrome): the element's remote object gets the files through
    /// DOM.setFileInputFiles; the reply names what the input holds.
    #[tokio::test]
    async fn chrome_sets_the_files_on_the_element() {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |params| {
            let expression = params["expression"].as_str().unwrap_or_default();
            if expression.contains("'not_file'") {
                json!({"result": {"result": {"type": "object", "value": {"status": "ok", "multiple": false}}}})
            } else if !expression.contains("f.name") {
                json!({"result": {"result": {"type": "object", "objectId": "o1"}}})
            } else {
                json!({"result": {"result": {"type": "object", "value": ["Cargo.toml"]}}})
            }
        });
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let args = json!({"selector": "#up", "paths": [manifest()]});
        let params = crate::handler::cdp_domains::tests::call("upload_file", args);
        let reply = UploadFileTool::handle(params, &handler).await.unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.starts_with("Set 1 file(s) on '#up': Cargo.toml."),
            "{text}"
        );
        let set = &mock.commands_named("DOM.setFileInputFiles")[0].params;
        assert_eq!(set, &json!({"files": [manifest()], "objectId": "o1"}));
        assert_eq!(mock.commands_named("Runtime.releaseObject").len(), 1);
    }
}
