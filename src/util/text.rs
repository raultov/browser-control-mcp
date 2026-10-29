//! Text helpers that must never panic on multi-byte input.

/// `text` within `max` characters: its start, then a note naming the total
/// (observation buffers keep a bounded copy of each item).
pub fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let start: String = text.chars().take(max).collect();
    format!("{start}… [cut: the first {max} of {total} characters]")
}

/// The note for `count` iframes of another origin, which the page cannot
/// read into the document.
fn frames_note(count: u64) -> Option<String> {
    let noun = if count == 1 { "iframe" } else { "iframes" };
    (count > 0).then(|| {
        format!("[{count} cross-origin {noun} not included: open their URL in a tab to read them.]")
    })
}

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
    let skipped = frames_note(outcome["skipped_frames"].as_u64().unwrap_or_default());
    let Some(query) = query else {
        let html = text("html");
        let shown = html.encode_utf16().count() as u64;
        let cut = (shown < total).then(|| format!(
            "[Truncated: the first {shown} of {total} characters of HTML. Pass 'query' to find a part, or read parts with 'evaluate_js'.]"
        ));
        let notes: Vec<String> = skipped.into_iter().chain(cut).collect();
        if notes.is_empty() {
            return html.to_string();
        }
        return format!("{}\n{html}", notes.join("\n"));
    };
    if outcome["found"].as_bool() != Some(true) {
        let missing = format!("Query '{query}' not found in the DOM.");
        return match skipped {
            Some(note) => format!("{missing}\n{note}"),
            None => missing,
        };
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

    /// AUD-OB-13: a long text keeps its start and says how much was cut.
    #[test]
    fn capped_keeps_the_start_and_names_the_total() {
        assert_eq!(capped("short", 10), "short");
        assert_eq!(
            capped("ééééé", 3),
            "ééé… [cut: the first 3 of 5 characters]"
        );
    }

    /// AUD-IN-27: the page sends at most `DOM_MAX_CHARS` of HTML; a cut
    /// document says so first. Positions are characters (AUD-IN-26).
    /// AUD-IN-24: iframes that could not be read (another origin) are
    /// named in every report.
    #[test]
    fn unreadable_frames_are_noted() {
        use serde_json::json;
        let note = "[2 cross-origin iframes not included: open their URL in a tab to read them.]";
        let whole = json!({"total": 5, "html": "<html>", "skipped_frames": 2});
        assert_eq!(dom_report(&whole, None, 1, 1), format!("{note}\n<html>"));
        let missing = json!({"total": 5, "found": false, "skipped_frames": 2});
        assert_eq!(
            dom_report(&missing, Some("x"), 1, 1),
            format!("Query 'x' not found in the DOM.\n{note}")
        );
        let one = json!({"total": 5, "html": "<html>", "skipped_frames": 1});
        assert!(dom_report(&one, None, 1, 1).starts_with("[1 cross-origin iframe not included"));
    }

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
