//! The problems the browser diagnoses but the console hides or words
//! vaguely: a cookie blocked and why, a CORS or CSP block, mixed content...
//! (ISSUE-60). Chrome reports them as structured issues (the `Audits`
//! domain, its DevTools Issues panel), kept per tab next to the console;
//! Firefox has no such panel, and reports them as security messages of
//! its console, read as issues here. `get_issues` lists them.

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::cdp_domains::log::{ConsoleMessage, LogState};
use crate::util::outline::{ELEMENT_HELPERS, ELEMENT_LOCATORS};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

/// Most issues a tab keeps (the oldest are forgotten).
pub(crate) const MAX_ISSUES: usize = 200;

/// Longest summary of an issue (characters).
const SUMMARY_MAX_CHARS: usize = 300;

/// Most distinct reports of one issue told apart from repeats; past them,
/// every report counts.
const MAX_REPORTS: usize = 100;

/// Most elements kept for one issue.
const MAX_ELEMENTS: usize = 10;

/// One problem the browser reported, folded with its other reports.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub(crate) struct Issue {
    /// Chrome's issue code (`CookieIssue`, `CorsIssue`...).
    pub(crate) code: String,
    /// What it is, in words, for the common Chrome codes (ISSUE-85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// What it says, on one line.
    pub(crate) summary: String,
    /// The resource it is about, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// The browser's own details (Chrome's `*IssueDetails`; Firefox's text),
    /// of its first report.
    pub(crate) details: Value,
    /// How many distinct reports it has (on Chrome, a report sent again
    /// with the same details is not counted).
    pub(crate) count: u32,
    /// The elements it is about (`ref`, `selector`, `snippet`), resolved
    /// when listed (Chrome, the tab's own issues).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) elements: Vec<Value>,
    /// Chrome's backend node ids of those elements, at most
    /// [`MAX_ELEMENTS`].
    #[serde(skip)]
    pub(crate) nodes: Vec<u64>,
    /// The CDP session that reported it (the tab's, or a frame's): node
    /// ids are only valid there.
    #[serde(skip)]
    pub(crate) session: Option<String>,
    /// Its reports' details (Chrome): one seen already is Chrome sending
    /// it again, not another occurrence (ISSUE-85).
    #[serde(skip)]
    reports: std::collections::BTreeSet<String>,
}

impl Issue {
    /// `[code] title: summary @ url [xN]`, then the elements it is about.
    fn line(&self) -> String {
        let at = self
            .url
            .as_deref()
            .map(|url| format!(" @ {url}"))
            .unwrap_or_default();
        let times = match self.count {
            0 | 1 => String::new(),
            n => format!(" [x{n}]"),
        };
        let title = self
            .title
            .as_deref()
            .map(|title| format!("{title}: "))
            .unwrap_or_default();
        let line = format!("[{}] {title}{}{at}{times}", self.code, self.summary);
        match self.elements.is_empty() {
            true => line,
            false => format!("{line}\n    elements: {}", self.element_list()),
        }
    }

    /// `selector [ref=eN], ...`.
    fn element_list(&self) -> String {
        let named = self.elements.iter().map(|element| {
            let selector = element["selector"].as_str().unwrap_or("?");
            match element["ref"].as_str() {
                Some(r) => format!("{selector} [ref={r}]"),
                None => selector.to_string(),
            }
        });
        named.collect::<Vec<_>>().join(", ")
    }

    /// Folds `other`, an issue with the same code, summary and URL: counted
    /// unless it is a report seen already, its elements added when both
    /// came from the same session.
    fn fold(&mut self, other: Issue) {
        let fresh =
            other.reports.is_empty() || other.reports.iter().any(|r| !self.reports.contains(r));
        if !fresh {
            return;
        }
        for report in other.reports {
            if self.reports.len() < MAX_REPORTS {
                self.reports.insert(report);
            }
        }
        self.count = self.count.saturating_add(other.count);
        if other.session == self.session {
            for node in other.nodes {
                if !self.nodes.contains(&node) && self.nodes.len() < MAX_ELEMENTS {
                    self.nodes.push(node);
                }
            }
        }
    }
}

/// The issue of a Chrome `Audits.issueAdded` (its `params`): its code and
/// its one `*IssueDetails` object.
pub(crate) fn from_chrome(params: &Value) -> Option<Issue> {
    let issue = params.get("issue")?;
    let code = issue.get("code")?.as_str()?.to_string();
    let details = issue
        .get("details")
        .and_then(Value::as_object)
        .and_then(|details| details.values().next())
        .cloned()
        .unwrap_or(Value::Null);
    let (summary, url) = summarize(&code, &details);
    let summary = crate::util::text::capped(&summary, SUMMARY_MAX_CHARS);
    let nodes = ["/violatingNodeId", "/nodeId"]
        .iter()
        .filter_map(|pointer| details.pointer(pointer).and_then(Value::as_u64))
        .take(1)
        .collect();
    Some(Issue {
        title: title(&code, &details).map(str::to_string),
        summary,
        url,
        reports: std::iter::once(details.to_string()).collect(),
        details,
        count: 1,
        nodes,
        code,
        ..Default::default()
    })
}

/// What an issue of `code` is, in words, for the common codes (a
/// `GenericIssue` by its `errorType`).
fn title(code: &str, details: &Value) -> Option<&'static str> {
    if code == "GenericIssue" {
        return form_title(text_at(details, "/errorType")?);
    }
    Some(match code {
        "CookieIssue" => "A cookie was blocked or warned",
        "CorsIssue" => "A cross-origin request was blocked (CORS)",
        "ContentSecurityPolicyIssue" => "The Content Security Policy blocked something",
        "MixedContentIssue" => "An insecure (http) resource on a secure page",
        "DeprecationIssue" => "A deprecated feature was used",
        "LowTextContrastIssue" => "Text contrast is too low",
        "QuirksModeIssue" => "The page renders in quirks mode (no <!doctype html>)",
        "HeavyAdIssue" => "An ad used too many resources and was removed",
        "SharedArrayBufferIssue" => "SharedArrayBuffer needs cross-origin isolation",
        "BlockedByResponseIssue" => "A resource was blocked by its response headers",
        _ => return None,
    })
}

/// What a `GenericIssue` of `kind` (its `errorType`) is, for the form ones.
fn form_title(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "FormEmptyIdAndNameAttributesForInputError" => "A form field has neither an id nor a name",
        "FormDuplicateIdForInputError" => "Form fields share an id",
        "FormLabelForMatchesNonExistingIdError" => "A <label for> names no element",
        "FormLabelForNameError" => "A <label for> names a field's name instead of its id",
        "FormInputWithNoLabelError" => "A form field has no label",
        "FormLabelHasNeitherForNorNestedInputError" => "A <label> has no for and no field inside",
        "FormAriaLabelledByToNonExistingIdError" => "aria-labelledby names no element",
        "FormAutocompleteAttributeEmptyError" => "An autocomplete attribute is empty",
        "FormInputHasWrongButWellIntendedAutocompleteValueError" => {
            "An autocomplete value is misspelled"
        }
        "FormInputAssignedAutocompleteValueToIdOrNameAttributeError" => {
            "An autocomplete value is in the field's id or name instead"
        }
        _ => return None,
    })
}

/// What an issue of `code` with `details` says, and the resource it is
/// about: the common codes by hand, the others by their short fields.
fn summarize(code: &str, details: &Value) -> (String, Option<String>) {
    match code {
        "CookieIssue" => cookie_summary(details),
        "CorsIssue" => {
            let error = text_at(details, "/corsErrorStatus/corsError").unwrap_or("CORS error");
            let summary = match text_at(details, "/corsErrorStatus/failedParameter") {
                Some(parameter) => format!("{error} ({parameter})"),
                None => error.to_string(),
            };
            (summary, owned(text_at(details, "/request/url")))
        }
        "ContentSecurityPolicyIssue" => {
            let directive = text_at(details, "/violatedDirective").unwrap_or("a directive");
            let report = match details["isReportOnly"].as_bool() {
                Some(true) => " (report only)",
                _ => "",
            };
            let blocked = text_at(details, "/blockedURL");
            let url = blocked.or(text_at(details, "/sourceCodeLocation/url"));
            (format!("{directive} blocked it{report}"), owned(url))
        }
        "MixedContentIssue" => {
            let status = text_at(details, "/resolutionStatus").unwrap_or("mixed content");
            let kind = text_at(details, "/resourceType").map(|k| format!(" {k}"));
            let summary = format!("{status}{}", kind.unwrap_or_default());
            (summary, owned(text_at(details, "/insecureURL")))
        }
        _ => generic_summary(details),
    }
}

/// The non-empty text at `pointer` in `details`.
fn text_at<'a>(details: &'a Value, pointer: &str) -> Option<&'a str> {
    details
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn owned(text: Option<&str>) -> Option<String> {
    text.map(str::to_string)
}

/// A `CookieIssue`: its exclusion and warning reasons, the operation and
/// the cookie, and the URL it was set or read for.
fn cookie_summary(details: &Value) -> (String, Option<String>) {
    let reasons: Vec<&str> = ["/cookieExclusionReasons", "/cookieWarningReasons"]
        .iter()
        .filter_map(|p| details.pointer(p).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let cookie = text_at(details, "/cookie/name")
        .or(text_at(details, "/rawCookieLine"))
        .unwrap_or("?");
    let operation = text_at(details, "/operation").unwrap_or("cookie");
    let url = text_at(details, "/cookieUrl").or(text_at(details, "/request/url"));
    let summary = format!("{}: {operation} cookie \"{cookie}\"", reasons.join(", "));
    (summary, owned(url))
}

/// Most short fields a summary of an issue of another code lists.
const GENERIC_FIELDS: usize = 3;

/// An issue of another code: its first short text fields (not ids, not
/// URLs) and its first URL.
fn generic_summary(details: &Value) -> (String, Option<String>) {
    let mut leaves = Vec::new();
    collect_leaves(details, "", &mut leaves, 0);
    let is_url = |key: &str| key.to_ascii_lowercase().ends_with("url");
    let url = leaves
        .iter()
        .find(|(key, _)| is_url(key))
        .map(|(_, v)| v.clone());
    let words: Vec<String> = leaves
        .into_iter()
        .filter(|(key, value)| {
            let id = key.ends_with("Id") || key.ends_with("id");
            !id && !is_url(key) && value.len() <= 80
        })
        .take(GENERIC_FIELDS)
        .map(|(_, value)| value)
        .collect();
    (words.join(", "), url)
}

/// The text leaves of `value` (two levels deep), with their keys.
fn collect_leaves(value: &Value, key: &str, out: &mut Vec<(String, String)>, depth: u32) {
    match value {
        Value::String(text) if !text.is_empty() => out.push((key.to_string(), text.clone())),
        Value::Object(fields) if depth <= 2 => {
            for (name, field) in fields {
                collect_leaves(field, name, out, depth + 1);
            }
        }
        Value::Array(items) if depth <= 2 => {
            for item in items {
                collect_leaves(item, key, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// The issue a Firefox console message reports, if it is one of its
/// security messages (cookies, CSP, mixed content, CORS); a message BiDi
/// reports without a stack (`other`) only when it reads as one.
pub(crate) fn from_firefox(message: &ConsoleMessage) -> Option<Issue> {
    if !matches!(message.source.as_str(), "security" | "other") {
        return None;
    }
    let text = message.text.as_str();
    let lower = text.to_lowercase();
    let code = if lower.starts_with("cookie") {
        "CookieIssue"
    } else if lower.starts_with("content-security-policy")
        || lower.contains("content security policy")
    {
        "ContentSecurityPolicyIssue"
    } else if lower.starts_with("cross-origin request blocked") {
        "CorsIssue"
    } else if lower.contains("mixed") && lower.contains("content") {
        "MixedContentIssue"
    } else if message.source == "other" && lower.contains("deprecated") {
        // Firefox's own deprecation warnings (not the page's console).
        "DeprecationIssue"
    } else if message.source == "security" {
        "SecurityIssue"
    } else {
        return None;
    };
    Some(Issue {
        code: code.to_string(),
        summary: crate::util::text::capped(text, SUMMARY_MAX_CHARS),
        url: message.url.clone(),
        details: json!({"level": message.level, "text": text}),
        count: 1,
        ..Default::default()
    })
}

/// The issues kept for a tab.
#[derive(Debug, Default)]
pub(crate) struct IssueBuffer {
    issues: std::collections::VecDeque<Issue>,
}

impl IssueBuffer {
    /// Keeps `issue`, folding it into an equal one kept before.
    pub(crate) fn push(&mut self, issue: Issue) {
        let same = |kept: &&mut Issue| {
            kept.code == issue.code && kept.summary == issue.summary && kept.url == issue.url
        };
        if let Some(kept) = self.issues.iter_mut().find(same) {
            kept.fold(issue);
            return;
        }
        self.issues.push_back(issue);
        if self.issues.len() > MAX_ISSUES {
            self.issues.pop_front();
        }
    }

    /// The issues, oldest first.
    pub(crate) fn list(&self) -> Vec<Issue> {
        self.issues.iter().cloned().collect()
    }

    /// Forgets the issues `code` matches (every one without a code).
    pub(crate) fn clear(&mut self, code: Option<&str>) {
        self.issues.retain(|issue| !code_matches(code, &issue.code));
    }

    /// Forgets them all (a new site).
    pub(crate) fn reset(&mut self) {
        self.issues.clear();
    }
}

/// Whether issue code `code` is the one `filter` asks for (a
/// case-insensitive substring; every code without a filter).
fn code_matches(filter: Option<&str>, code: &str) -> bool {
    filter.is_none_or(|filter| code.to_lowercase().contains(&filter.to_lowercase()))
}

#[macros::mcp_tool(
    name = "get_issues",
    description = "Lists the problems the browser diagnosed in the tab that the console hides or words vaguely: blocked or warned cookies and why, CORS and Content-Security-Policy blocks, mixed content, deprecated features, and on Chrome every code of its DevTools Issues panel (form fields without id, name or label, LazyLoadImageIssue...). One line each, oldest first: [code] title: summary @ url [xN] (a title for the common Chrome codes; N distinct reports, one per element or request), then on Chrome its elements (selector [ref=eN], refs as snapshot's, at most 10). For console messages use get_console_logs. Firefox has no Issues panel: its security and deprecation console messages are read as issues (cookies, CSP, mixed content, CORS, DeprecationIssue; needs its debugger port)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetIssuesTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// Only the issues whose code contains this (case-insensitive), e.g. 'Cookie'.
    pub code: Option<String>,
    /// The browser's details of each issue, as JSON. Default: false.
    pub include_details: Option<bool>,
    /// Remove the returned issues. Default: false.
    pub clear: Option<bool>,
}

impl GetIssuesTool {
    /// The reply listing `issues` (filtered already), with `notes` last.
    pub(crate) fn reply(&self, issues: &[Issue], notes: &[String]) -> CallToolResult {
        let text = if self.include_details == Some(true) {
            let mut body = json!({"issues": issues});
            if !notes.is_empty() {
                body["notes"] = json!(notes);
            }
            serde_json::to_string_pretty(&body).unwrap_or_default()
        } else {
            let mut lines = vec![match issues.len() {
                0 => "0 issue(s): the browser diagnosed no problem in this tab.".to_string(),
                n => format!("{n} issue(s), oldest first:"),
            }];
            lines.extend(issues.iter().map(Issue::line));
            lines.extend(notes.iter().map(|note| format!("Note: {note}")));
            lines.join("\n")
        };
        CallToolResult::text_content(vec![text.into()])
    }

    /// `get_issues` on a Chrome instance: the tab's `Audits` issues.
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &crate::handler::McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        // Connected first (Audits.enable replays the page's issues).
        let first = !session.is_connected().await;
        let target = session.target(args.tab_id.clone()).await?;
        if first {
            session.settle_replayed_events().await;
        }
        let log = session.log_state(args.tab_id.clone())?;
        let code = args.code.as_deref();
        let mut issues: Vec<Issue> = {
            let mut log = log.lock().await;
            let issues = log.issues.list();
            if args.clear == Some(true) {
                log.issues.clear(code);
            }
            issues
        };
        issues.retain(|issue| code_matches(code, &issue.code));
        let tab = target.page_tab().map(|tab| tab.session_id().to_string());
        resolve_elements(&target, tab.as_deref(), &mut issues).await;
        Ok(args.reply(&issues, &[]))
    }
}

/// Most elements `get_issues` resolves in one call (two commands each).
const MAX_RESOLVED: usize = 50;

/// The object group of the elements `get_issues` resolves, released after.
const LOCATE_GROUP: &str = "browser-control-mcp-issues";

/// Fills the elements of the issues the tab's session (`tab`: `None` for a
/// connection to the page itself) reported from their backend node ids: their ref, selector and opening tag, as
/// `audit_page` gives them. A frame's node ids belong to its own process,
/// and a node of a document that is gone no longer resolves: both are
/// left out (ISSUE-85).
async fn resolve_elements(target: &CdpTarget, tab: Option<&str>, issues: &mut [Issue]) {
    let ours = |issue: &Issue| issue.session.as_deref() == tab;
    if !issues
        .iter()
        .any(|issue| ours(issue) && !issue.nodes.is_empty())
    {
        return;
    }
    let locate = format!(
        "function() {{ if (this.nodeType !== 1) return null; {ELEMENT_HELPERS} {ELEMENT_LOCATORS} return {{ ref: refOf(this), selector: selectorOf(this), snippet: snippetOf(this) }}; }}"
    );
    let mut budget = MAX_RESOLVED;
    for issue in issues.iter_mut() {
        if !ours(issue) {
            continue;
        }
        let nodes: Vec<u64> = issue.nodes.iter().copied().take(budget).collect();
        budget -= nodes.len();
        for node in nodes {
            issue
                .elements
                .extend(locate_node(target, node, &locate).await);
        }
    }
    let release = json!({ "objectGroup": LOCATE_GROUP });
    let _ = target
        .send_raw_command("Runtime.releaseObjectGroup", release)
        .await;
}

/// Backend node `node` located by `locate` (`{ref, selector, snippet}`),
/// if it still is an element of the page.
async fn locate_node(target: &CdpTarget, node: u64, locate: &str) -> Option<Value> {
    let resolve = json!({"backendNodeId": node, "objectGroup": LOCATE_GROUP});
    let resolved = target.send_raw_command("DOM.resolveNode", resolve).await;
    let object = resolved.ok()?.result?["object"]["objectId"].clone();
    let call =
        json!({"objectId": object.as_str()?, "functionDeclaration": locate, "returnByValue": true});
    let reply = target
        .send_raw_command("Runtime.callFunctionOn", call)
        .await;
    let value = reply.ok()?.result?["result"]["value"].clone();
    value.is_object().then_some(value)
}

/// The issues of a Firefox tab's console (`log`), folded, after its last
/// `clear`; `clear` moves that mark past them.
pub(crate) fn firefox_issues(args: &GetIssuesTool, log: &mut LogState) -> Vec<Issue> {
    let code = args.code.as_deref();
    let mut folded = IssueBuffer::default();
    let mut returned: std::collections::BTreeMap<String, f64> = Default::default();
    for message in &log.messages {
        let Some(issue) = from_firefox(message).filter(|i| code_matches(code, &i.code)) else {
            continue;
        };
        let cleared = log.issues_cleared_at.get(&issue.code);
        if cleared.is_some_and(|at| message.timestamp <= *at) {
            continue;
        }
        let last = returned
            .entry(issue.code.clone())
            .or_insert(message.timestamp);
        *last = last.max(message.timestamp);
        folded.push(issue);
    }
    if args.clear == Some(true) {
        log.issues_cleared_at.extend(returned);
    }
    folded.list()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::log::Channel;

    fn text_of(reply: &CallToolResult) -> String {
        match &reply.content[0] {
            rust_mcp_sdk::schema::ContentBlock::TextContent(t) => t.text.clone(),
            _ => String::new(),
        }
    }

    /// ISSUE-60 (Chrome): the common issues are summarized by what they
    /// say (the cookie's reasons and operation, the CORS error, the CSP
    /// directive), with the resource they are about; another code by its
    /// short fields; the details are kept.
    #[test]
    fn chrome_issues_are_summarized() {
        let issue = |code: &str, details: Value| {
            from_chrome(&json!({"issue": {"code": code, "details": details}})).unwrap()
        };
        let cookie = issue(
            "CookieIssue",
            json!({"cookieIssueDetails": {"cookie": {"domain": "a", "name": "srv", "path": "/"},
            "cookieExclusionReasons": ["ExcludeSameSiteNoneInsecure"], "cookieWarningReasons": [], "cookieUrl": "http://a/x", "operation": "SetCookie"}}),
        );
        assert_eq!(
            cookie.line(),
            "[CookieIssue] A cookie was blocked or warned: ExcludeSameSiteNoneInsecure: SetCookie cookie \"srv\" @ http://a/x"
        );
        assert_eq!(cookie.details["cookie"]["name"], "srv");
        let cors = issue(
            "CorsIssue",
            json!({"corsIssueDetails": {"corsErrorStatus": {"corsError": "MissingAllowOriginHeader", "failedParameter": ""},
            "request": {"requestId": "1.4", "url": "http://b/api"}, "isWarning": false}}),
        );
        assert_eq!(
            cors.line(),
            "[CorsIssue] A cross-origin request was blocked (CORS): MissingAllowOriginHeader @ http://b/api"
        );
        let csp = issue(
            "ContentSecurityPolicyIssue",
            json!({"contentSecurityPolicyIssueDetails": {"blockedURL": "http://a/img",
            "violatedDirective": "img-src", "isReportOnly": true, "sourceCodeLocation": {"url": "http://a/", "lineNumber": -1}}}),
        );
        assert_eq!(
            csp.line(),
            "[ContentSecurityPolicyIssue] The Content Security Policy blocked something: img-src blocked it (report only) @ http://a/img"
        );
        let mixed = issue(
            "MixedContentIssue",
            json!({"mixedContentIssueDetails": {"resolutionStatus": "MixedContentBlocked",
            "resourceType": "Image", "insecureURL": "http://a/i.png", "mainResourceURL": "https://a/"}}),
        );
        assert_eq!(
            mixed.line(),
            "[MixedContentIssue] An insecure (http) resource on a secure page: MixedContentBlocked Image @ http://a/i.png"
        );
    }

    /// ISSUE-60 (Chrome): an issue of another code is summarized by its
    /// short fields and first URL; one without a code is no issue.
    #[test]
    fn other_chrome_issues_are_summarized() {
        let issue = |code: &str, details: Value| {
            from_chrome(&json!({"issue": {"code": code, "details": details}})).unwrap()
        };
        let other = issue(
            "DeprecationIssue",
            json!({"deprecationIssueDetails": {"type": "UnloadHandler", "affectedFrame": {"frameId": "F"},
            "sourceCodeLocation": {"url": "http://a/app.js", "lineNumber": 3}}}),
        );
        assert_eq!(
            other.line(),
            "[DeprecationIssue] A deprecated feature was used: UnloadHandler @ http://a/app.js"
        );
        assert!(from_chrome(&json!({"issue": {}})).is_none());
    }

    /// ISSUE-60: an issue reported again is folded into the first; the
    /// buffer is bounded; `clear` with a code forgets only those.
    #[test]
    fn issues_are_folded_and_bounded() {
        let issue = |code: &str, summary: &str| Issue {
            code: code.into(),
            summary: summary.into(),
            url: None,
            details: Value::Null,
            count: 1,
            ..Default::default()
        };
        let mut buffer = IssueBuffer::default();
        buffer.push(issue("CookieIssue", "a"));
        buffer.push(issue("CookieIssue", "a"));
        buffer.push(issue("CorsIssue", "b"));
        let kept = buffer.list();
        assert_eq!((kept.len(), kept[0].count), (2, 2));
        assert_eq!(kept[0].line(), "[CookieIssue] a [x2]");
        for n in 0..MAX_ISSUES {
            buffer.push(issue("X", &n.to_string()));
        }
        assert_eq!(buffer.list().len(), MAX_ISSUES);
        buffer.clear(Some("x"));
        assert!(buffer.list().is_empty());
    }

    /// A Firefox console message of `source`.
    fn message(source: &str, text: &str) -> ConsoleMessage {
        ConsoleMessage {
            source: source.into(),
            level: "error".into(),
            text: text.into(),
            timestamp: 1.0,
            url: Some("http://a/".into()),
            line_number: None,
            channel: Channel::Watcher,
        }
    }

    /// ISSUE-60 (Firefox): its security messages are read as issues with
    /// Chrome's codes; other messages are not issues.
    #[test]
    fn firefox_security_messages_are_issues() {
        let code = |source: &str, text: &str| from_firefox(&message(source, text)).map(|i| i.code);
        assert_eq!(
            code(
                "security",
                "Cookie “a” rejected because it has the “SameSite=None” attribute"
            )
            .as_deref(),
            Some("CookieIssue")
        );
        assert_eq!(
            code(
                "other",
                "Cookie “srv” has been rejected because it is in a cross-site context"
            )
            .as_deref(),
            Some("CookieIssue")
        );
        assert_eq!(
            code(
                "security",
                "Content-Security-Policy: The page’s settings blocked the loading of a resource"
            )
            .as_deref(),
            Some("ContentSecurityPolicyIssue")
        );
        assert_eq!(
            code(
                "security",
                "Cross-Origin Request Blocked: The Same Origin Policy disallows reading"
            )
            .as_deref(),
            Some("CorsIssue")
        );
        assert_eq!(
            code(
                "security",
                "Blocked loading mixed active content “http://a/x.js”"
            )
            .as_deref(),
            Some("MixedContentIssue")
        );
        assert_eq!(
            code("security", "Something else secure").as_deref(),
            Some("SecurityIssue")
        );
        assert_eq!(code("console-api", "Cookie monster"), None);
        assert_eq!(code("other", "Gecko platform message"), None);
    }

    /// ISSUE-60 (Firefox): an issue's summary is the message's text, and its
    /// URL the message's.
    #[test]
    fn firefox_issues_keep_the_message() {
        let issue = from_firefox(&message("security", "Cookie “a” rejected")).unwrap();
        assert_eq!(
            (issue.summary.as_str(), issue.url.as_deref()),
            ("Cookie “a” rejected", Some("http://a/"))
        );
    }

    /// ISSUE-60 (Firefox): the issues come from the tab's console, folded;
    /// `clear` hides those returned from the next call.
    #[test]
    fn firefox_issues_follow_the_console() {
        let mut log = LogState::default();
        let mut push = |text: &str, at: f64| {
            log.push_message(ConsoleMessage {
                source: "security".into(),
                level: "error".into(),
                text: text.into(),
                timestamp: at,
                url: None,
                line_number: None,
                channel: Channel::Watcher,
            });
        };
        push("Cookie “a” rejected", 1.0);
        push("Cookie “a” rejected", 2.0);
        push("Cross-Origin Request Blocked: x", 3.0);
        let cookies: GetIssuesTool =
            serde_json::from_value(json!({"clear": true, "code": "cookie"})).unwrap();
        let issues = firefox_issues(&cookies, &mut log);
        assert_eq!((issues.len(), issues[0].count), (1, 2));
        assert!(firefox_issues(&cookies, &mut log).is_empty());
        // And clearing one code keeps the others
        let all: GetIssuesTool = serde_json::from_value(json!({})).unwrap();
        let left: Vec<String> = firefox_issues(&all, &mut log)
            .into_iter()
            .map(|i| i.code)
            .collect();
        assert_eq!(left, ["CorsIssue"]);
    }

    /// A Chrome form issue about backend node `node`, from session `S`.
    fn form_issue(kind: &str, node: u64) -> Issue {
        let details = json!({"genericIssueDetails": {"errorType": kind, "violatingNodeId": node, "violatingNodeAttribute": "id"}});
        let mut issue =
            from_chrome(&json!({"issue": {"code": "GenericIssue", "details": details}})).unwrap();
        issue.session = Some("S".into());
        issue
    }

    /// ISSUE-85 (Chrome): Chrome sends its form issues twice, with the same
    /// details: a repeat is not counted (the counts were doubled); each
    /// distinct report is, and keeps the element it is about.
    #[test]
    fn identical_chrome_reports_count_once() {
        // Given Chrome's two reports of each of two duplicate-id fields
        let mut buffer = IssueBuffer::default();
        for node in [6, 7, 6, 7] {
            buffer.push(form_issue("FormDuplicateIdForInputError", node));
        }
        // Then one issue counts the two fields, and keeps both
        let kept = buffer.list();
        assert_eq!((kept.len(), kept[0].count), (1, 2));
        assert_eq!(kept[0].nodes, [6, 7]);
        // And its line says what it is about, not only Chrome's code
        assert_eq!(
            kept[0].line(),
            "[GenericIssue] Form fields share an id: FormDuplicateIdForInputError, id [x2]"
        );
        // And once cleared, a report seen before counts again
        buffer.clear(None);
        buffer.push(form_issue("FormDuplicateIdForInputError", 6));
        assert_eq!(buffer.list()[0].count, 1);
    }

    /// ISSUE-85: the common codes carry a title; others do not.
    #[test]
    fn common_issues_have_a_title() {
        let titled = |code: &str, details: Value| {
            from_chrome(&json!({"issue": {"code": code, "details": {"d": details}}}))
                .unwrap()
                .title
        };
        assert_eq!(
            titled(
                "GenericIssue",
                json!({"errorType": "FormLabelForMatchesNonExistingIdError"})
            )
            .as_deref(),
            Some("A <label for> names no element")
        );
        assert_eq!(
            titled("DeprecationIssue", json!({"type": "X"})).as_deref(),
            Some("A deprecated feature was used")
        );
        assert_eq!(titled("SomeNewIssue", json!({})), None);
    }

    /// ISSUE-85: the elements an issue is about follow its line (selector
    /// and ref), and are in its details.
    #[test]
    fn replies_list_the_elements() {
        let mut issue = form_issue("FormDuplicateIdForInputError", 6);
        issue.elements = vec![
            json!({"ref": "e5", "selector": "form > input:nth-of-type(3)", "snippet": "<input id=\"dup\">"}),
        ];
        let args: GetIssuesTool = serde_json::from_value(json!({})).unwrap();
        let text = text_of(&args.reply(std::slice::from_ref(&issue), &[]));
        assert!(
            text.ends_with("\n    elements: form > input:nth-of-type(3) [ref=e5]"),
            "{text}"
        );
        let detailed: GetIssuesTool =
            serde_json::from_value(json!({"include_details": true})).unwrap();
        let body: Value = serde_json::from_str(&text_of(&detailed.reply(&[issue], &[]))).unwrap();
        assert_eq!(body["issues"][0]["elements"][0]["ref"], "e5");
    }

    /// ISSUE-85 (Chrome): get_issues resolves the elements of the tab's own
    /// issues to refs and selectors; a frame's node ids are another
    /// process's, so its issues are left as they are.
    #[tokio::test]
    async fn the_tabs_issue_elements_are_resolved() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a tab issue about node 6 and a frame issue about node 9
        let mock = spawn_recording_mock().await;
        mock.respond_with("DOM.resolveNode", |_| json!({"object": {"objectId": "o1"}}));
        mock.respond_with("Runtime.callFunctionOn", |_| {
            json!({"result": {"result": {"type": "object", "value": {"ref": "e5", "selector": "form > input:nth-of-type(3)", "snippet": "<input>"}}}})
        });
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        // (a registered tab: a flat session of its own, as in a real browser)
        let id =
            crate::handler::cdp_domains::tests::register_tab_on(&session, mock.http_port).await;
        let target = session.target(Some(id.clone())).await.unwrap();
        let tab = target.page_tab().map(|tab| tab.session_id().to_string());
        assert!(tab.is_some());
        let log = session.log_state(Some(id.clone())).unwrap();
        let mut ours = form_issue("FormDuplicateIdForInputError", 6);
        ours.session = tab.clone();
        let mut framed = form_issue("FormLabelForMatchesNonExistingIdError", 9);
        framed.session = Some("FRAME".into());
        log.lock().await.issues.push(ours);
        log.lock().await.issues.push(framed);
        // When get_issues runs
        let reply = GetIssuesTool::handle(call("get_issues", json!({"tab_id": id})), &handler)
            .await
            .unwrap();
        // Then the tab's element is named, and only it was resolved
        let text = reply_text(&reply);
        assert!(
            text.contains("elements: form > input:nth-of-type(3) [ref=e5]"),
            "{text}"
        );
        let resolved = mock.commands_named("DOM.resolveNode");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].params["backendNodeId"], 6);
    }

    /// ISSUE-85 (Firefox): its deprecation warnings are issues; a page's
    /// own console.warn saying "deprecated" is not.
    #[test]
    fn firefox_deprecation_warnings_are_issues() {
        let text = "Synchronous XMLHttpRequest on the main thread is deprecated because of its detrimental effects";
        let code = |source: &str| from_firefox(&message(source, text)).map(|i| i.code);
        assert_eq!(code("other").as_deref(), Some("DeprecationIssue"));
        assert_eq!(code("console-api"), None);
    }

    /// ISSUE-60: the reply is one line per issue, or the details as JSON.
    #[test]
    fn replies_list_the_issues() {
        let issue = Issue {
            code: "CorsIssue".into(),
            summary: "MissingAllowOriginHeader".into(),
            url: Some("http://b/api".into()),
            details: json!({"isWarning": false}),
            count: 1,
            ..Default::default()
        };
        let args: GetIssuesTool = serde_json::from_value(json!({})).unwrap();
        let notes = ["a note".to_string()];
        let text = text_of(&args.reply(std::slice::from_ref(&issue), &notes));
        assert_eq!(
            text,
            "1 issue(s), oldest first:\n[CorsIssue] MissingAllowOriginHeader @ http://b/api\nNote: a note"
        );
        assert_eq!(
            text_of(&args.reply(&[], &[])),
            "0 issue(s): the browser diagnosed no problem in this tab."
        );
        let detailed: GetIssuesTool =
            serde_json::from_value(json!({"include_details": true})).unwrap();
        let body: Value = serde_json::from_str(&text_of(&detailed.reply(&[issue], &[]))).unwrap();
        assert_eq!(body["issues"][0]["details"]["isWarning"], false);
    }
}
