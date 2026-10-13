//! JavaScript snippets the tools evaluate in the page, shared by every
//! backend. Values from the agent reach JavaScript only as JSON string
//! literals, never by hand-escaping (a `\` used to break out of the string).

/// `value` as a JavaScript string literal (JSON strings are valid JS).
pub fn string_literal(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// `(function() { const el = <selector>; if (!el) return <missing>; <body> })()`:
/// the one shape every element snippet has.
fn on_element(selector: &str, missing: &str, body: &str) -> String {
    format!(
        "(function() {{ const el = document.querySelector({}); if (!el) return {missing}; {body} }})()",
        string_literal(selector)
    )
}

/// Scrolls the first element matching `selector` into view; `true` if found.
pub fn scroll_into_view(selector: &str) -> String {
    on_element(
        selector,
        "false",
        "el.scrollIntoView({ behavior: 'instant', block: 'center' }); return true;",
    )
}

/// The viewport centre `{x, y}` of the first element matching `selector`, or `null`.
pub fn element_center(selector: &str) -> String {
    on_element(
        selector,
        "null",
        "const rect = el.getBoundingClientRect(); \
         return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };",
    )
}

/// Focuses the first element matching `selector` and empties it, so the
/// text typed next replaces its content; `true` if found.
pub fn focus_and_clear(selector: &str) -> String {
    on_element(
        selector,
        "false",
        "el.focus(); \
         if ('value' in el) { el.value = ''; } else if (el.isContentEditable) { el.textContent = ''; } \
         el.dispatchEvent(new Event('input', { bubbles: true })); return true;",
    )
}

/// The page metrics JavaScript can read without injection (Firefox's
/// `get_performance_metrics`), named like Chrome's `Performance.getMetrics`
/// where they mean the same: `Nodes` (nodes of the document tree), `Frames`
/// (the page and its iframes), `Resources` (resource timing entries) and
/// `TransferSize` (bytes over the network for the document and its
/// resources, from resource timing).
pub const PAGE_METRICS: &str = "(() => { \
    let nodes = 1; const walker = document.createTreeWalker(document, NodeFilter.SHOW_ALL); \
    while (walker.nextNode()) nodes++; \
    const resources = performance.getEntriesByType('resource'); \
    const timed = resources.concat(performance.getEntriesByType('navigation')); \
    const transfer = timed.reduce((sum, e) => sum + (e.transferSize || 0), 0); \
    return { Nodes: nodes, Frames: window.frames.length + 1, Resources: resources.length, TransferSize: transfer }; \
})()";

/// The page-load vitals the Web Performance APIs give (Firefox's
/// `profile_page_performance`), in ms since the navigation started: DCL and
/// load from navigation timing, FCP from paint timing, LCP from a buffered
/// `largest-contentful-paint` observer (`null` when not reported), plus
/// whether the browser has the Long Tasks API.
pub const PAGE_VITALS: &str = "(() => { \
    const nav = performance.getEntriesByType('navigation')[0]; \
    const fcp = performance.getEntriesByName('first-contentful-paint')[0]; \
    const types = (window.PerformanceObserver && PerformanceObserver.supportedEntryTypes) || []; \
    let lcp = null; \
    if (types.includes('largest-contentful-paint')) { \
        const observer = new PerformanceObserver(() => {}); \
        observer.observe({ type: 'largest-contentful-paint', buffered: true }); \
        const entries = observer.takeRecords(); observer.disconnect(); \
        if (entries.length) lcp = entries[entries.length - 1].startTime; \
    } \
    return { \
        dom_content_loaded_ms: nav && nav.domContentLoadedEventEnd > 0 ? nav.domContentLoadedEventEnd : null, \
        load_event_ms: nav && nav.loadEventEnd > 0 ? nav.loadEventEnd : null, \
        first_contentful_paint_ms: fcp ? fcp.startTime : null, \
        largest_contentful_paint_ms: lcp, \
        lcp_supported: types.includes('largest-contentful-paint'), \
        long_tasks_supported: types.includes('longtask'), \
    }; \
})()";

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument of `document.querySelector(..)` in `expression`, parsed back as JSON.
    fn embedded_selector(expression: &str) -> String {
        let start = expression.find("querySelector(").unwrap() + "querySelector(".len();
        let mut values = serde_json::Deserializer::from_str(&expression[start..]).into_iter();
        let selector: String = values.next().unwrap().unwrap();
        // The literal must end right where the call does.
        assert!(expression[start + values.byte_offset()..].starts_with(");"));
        selector
    }

    /// SEC-06: Selectors cannot break out of the JavaScript string.
    #[test]
    fn selector_is_embedded_as_one_json_string() {
        // Given a selector crafted to close the string and run code
        let selector = r#"a\"); window.__pwned = 1; (""#;
        // When each snippet embeds it
        for expression in [
            scroll_into_view(selector),
            element_center(selector),
            focus_and_clear(selector),
        ] {
            // Then it is one JSON string literal that parses back to the original
            assert_eq!(embedded_selector(&expression), selector, "{expression}");
        }
    }

    /// FIX-07: fill_input replaces the existing value.
    #[test]
    fn fill_input_replaces_value() {
        let expression = focus_and_clear("#q");
        assert!(expression.contains("el.focus()"));
        assert!(expression.contains("el.value = ''"), "{expression}");
    }
}
