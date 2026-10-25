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

/// Readies the first element matching `selector` for `text` (`fill_input`).
/// A text field or contenteditable element is focused and emptied (through
/// the native `value` setter, which frameworks such as React observe), so
/// the text typed next replaces its content; a `<select>` gets the option
/// whose value or label is `text`. Returns `{status}`: `missing`,
/// `not_editable` (with `reason`), `no_option` (with `options`), `selected`
/// or `ready`. Checking focus matters: typing goes to whatever element has
/// it, so an element that cannot take it would send the text elsewhere.
pub fn prepare_fill(selector: &str, text: &str) -> String {
    on_element(
        selector,
        "{ status: 'missing' }",
        &format!("const text = {}; {PREPARE_FILL}", string_literal(text)),
    )
}

const PREPARE_FILL: &str = "if (el instanceof HTMLSelectElement) { \
        if (el.matches(':disabled')) return { status: 'not_editable', reason: 'disabled' }; \
        const options = Array.from(el.options); \
        const option = options.find(o => o.value === text) || options.find(o => o.label.trim() === text.trim()); \
        if (!option) return { status: 'no_option', options: options.slice(0, 20).map(o => o.label.trim()) }; \
        option.selected = true; \
        el.dispatchEvent(new Event('input', { bubbles: true })); \
        el.dispatchEvent(new Event('change', { bubbles: true })); \
        return { status: 'selected' }; \
    } \
    const field = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement; \
    const typed = ['text', 'search', 'email', 'url', 'tel', 'password', 'number', 'date', 'datetime-local', 'month', 'week', 'time']; \
    if (el instanceof HTMLInputElement && !typed.includes(el.type)) return { status: 'not_editable', reason: 'an input of type ' + el.type }; \
    if (!field && !el.isContentEditable) return { status: 'not_editable', reason: 'not an input, textarea or contenteditable element' }; \
    if (field && el.matches(':disabled')) return { status: 'not_editable', reason: 'disabled' }; \
    if (field && el.readOnly) return { status: 'not_editable', reason: 'read-only' }; \
    el.focus(); \
    const active = document.activeElement; \
    if (active !== el && !(el.isContentEditable && active && active.contains(el))) return { status: 'not_editable', reason: 'it cannot take focus (hidden or inert)' }; \
    if (field) { \
        const value = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value'); \
        if (value && value.set) { value.set.call(el, ''); } else { el.value = ''; } \
    } else { el.textContent = ''; } \
    el.dispatchEvent(new Event('input', { bubbles: true })); \
    return { status: 'ready' };";

/// What the element matching `selector` holds after `text` was typed:
/// `{status: 'missing'}` or `{status: 'done', matches, empty}` (the value
/// itself never leaves the page, as it may be a password).
pub fn fill_result(selector: &str, text: &str) -> String {
    on_element(
        selector,
        "{ status: 'missing' }",
        &format!(
            "const text = {}; \
             const value = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement ? el.value : el.textContent; \
             return {{ status: 'done', matches: value === text, empty: value === '' }};",
            string_literal(text)
        ),
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
            prepare_fill(selector, selector),
            fill_result(selector, selector),
        ] {
            // Then it is one JSON string literal that parses back to the original
            assert_eq!(embedded_selector(&expression), selector, "{expression}");
        }
    }

    /// The `const text = <literal>;` of a fill snippet, parsed back as JSON.
    fn embedded_text(expression: &str) -> String {
        let start = expression.find("const text = ").unwrap() + "const text = ".len();
        let mut values = serde_json::Deserializer::from_str(&expression[start..]).into_iter();
        values.next().unwrap().unwrap()
    }

    /// FIX-07 / AUD-IN-08: fill_input empties the field it focused, and
    /// the text reaches JavaScript as one string literal.
    #[test]
    fn fill_input_replaces_value() {
        let text = r#"x"; window.__pwned = 1; ""#;
        let expression = prepare_fill("#q", text);
        assert!(expression.contains("el.focus()"));
        assert!(
            expression.contains("value.set.call(el, '')"),
            "{expression}"
        );
        assert!(
            expression.contains("document.activeElement"),
            "{expression}"
        );
        assert_eq!(embedded_text(&expression), text);
        assert_eq!(embedded_text(&fill_result("#q", text)), text);
    }
}
