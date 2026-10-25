//! Text helpers that must never panic on multi-byte input.

/// The `inspect_dom` text from the page's outcome (`util::js::dom_query`):
/// the document, after a note when it was cut, or the first match with its
/// context (same output on every browser).
pub fn dom_report(
    outcome: &serde_json::Value,
    query: Option<&str>,
    before: usize,
    after: usize,
) -> String {
    let text = |key: &str| outcome[key].as_str().unwrap_or_default();
    let total = outcome["total"].as_u64().unwrap_or_default();
    let Some(query) = query else {
        let html = text("html");
        let shown = html.encode_utf16().count() as u64;
        if shown >= total {
            return html.to_string();
        }
        return format!(
            "[Truncated: the first {shown} of {total} characters of HTML. Pass 'query' to find a part, or read parts with 'evaluate_js'.]\n{html}"
        );
    };
    if outcome["found"].as_bool() != Some(true) {
        return format!("Query '{query}' not found in the DOM.");
    }
    format!(
        "Found '{query}' at position {}. Context (before={before}, after={after}):\n\n[...]\n{}\n[...]",
        outcome["position"].as_u64().unwrap_or_default(),
        text("context")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD-IN-27: the page sends at most `DOM_MAX_CHARS` of HTML; a cut
    /// document says so first. Positions are characters (AUD-IN-26).
    #[test]
    fn dom_reports_from_the_page_outcome() {
        use serde_json::json;
        let whole = json!({"total": 11, "html": "<html></html>"});
        assert_eq!(dom_report(&whole, None, 200, 200), "<html></html>");
        let cut = json!({"total": 3_400_000, "html": "<html>"});
        let text = dom_report(&cut, None, 200, 200);
        assert!(
            text.starts_with("[Truncated: the first 6 of 3400000 characters"),
            "{text}"
        );
        assert!(text.ends_with("\n<html>"), "{text}");
        let found = json!({"total": 50, "found": true, "position": 7, "context": "ab needle cd"});
        assert_eq!(
            dom_report(&found, Some("needle"), 3, 3),
            "Found 'needle' at position 7. Context (before=3, after=3):\n\n[...]\nab needle cd\n[...]"
        );
        let missing = json!({"total": 50, "found": false});
        assert_eq!(
            dom_report(&missing, Some("x"), 3, 3),
            "Query 'x' not found in the DOM."
        );
    }
}
