//! Text helpers that must never panic on multi-byte input.

/// The slice of `text` from `before` bytes ahead of `matched` to `after`
/// bytes past it, widened to the nearest UTF-8 character boundaries so it can
/// never split a character (slicing by raw byte offsets panicked).
pub fn context_window(
    text: &str,
    matched: std::ops::Range<usize>,
    before: usize,
    after: usize,
) -> &str {
    let mut start = matched.start.saturating_sub(before).min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = matched.end.saturating_add(after).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    text.get(start..end).unwrap_or_default()
}

/// The `inspect_dom` text for `html`: the whole document without a query,
/// otherwise the first match with its context (same output on every browser).
pub fn dom_query_report(html: &str, query: Option<&str>, before: usize, after: usize) -> String {
    let Some(query) = query else {
        return html.to_string();
    };
    match html.find(query) {
        Some(pos) => format!(
            "Found '{}' at position {}. Context (before={}, after={}):\n\n[...]\n{}\n[...]",
            query,
            pos,
            before,
            after,
            context_window(html, pos..pos + query.len(), before, after)
        ),
        None => format!("Query '{}' not found in the DOM.", query),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SEC-12: inspect_dom never panics on multi-byte text.
    #[test]
    fn inspect_dom_slices_on_char_boundaries() {
        // Given a document of two-byte characters with "needle" in the middle
        let html = format!("{}needle{}", "ñ".repeat(250), "ñ".repeat(250));
        for width in 1..=7 {
            // When the query is reported with odd context widths
            let report = dom_query_report(&html, Some("needle"), width, width);
            // Then it returns the context around the match (no panic, no empty slice)
            let context = report.split("[...]\n").nth(1).unwrap_or_default();
            assert!(context.contains("needle"), "width {width}: {report}");
        }
    }

    #[test]
    fn context_window_clamps_to_the_text() {
        assert_eq!(context_window("abc", 1..2, 10, 10), "abc");
        assert_eq!(context_window("abcdef", 2..4, 1, 1), "bcde");
    }
}
