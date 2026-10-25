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

/// Scrolls the first element matching `selector` into view. Returns
/// `{status: 'missing'}`, `{status: 'hidden'}` (no layout box, nothing to
/// scroll to) or `{status: 'done', before, after, in_view}` with the window
/// positions, so the reply can tell what moved.
pub fn scroll_into_view(selector: &str) -> String {
    on_element(
        selector,
        "{ status: 'missing' }",
        "const before = { x: scrollX, y: scrollY }; \
         el.scrollIntoView({ behavior: 'instant', block: 'center' }); \
         const r = el.getBoundingClientRect(); \
         if (r.width === 0 && r.height === 0) return { status: 'hidden' }; \
         const in_view = r.bottom > 0 && r.top < innerHeight && r.right > 0 && r.left < innerWidth; \
         return { status: 'done', before, after: { x: scrollX, y: scrollY }, in_view };",
    )
}

/// Scrolls the window by `(left, top)` (JavaScript expressions) and returns
/// `{status: 'done', before, after}` with the window positions.
pub fn scroll_by(left: &str, top: &str) -> String {
    format!(
        "(function() {{ const before = {{ x: scrollX, y: scrollY }}; \
         window.scrollBy({{ left: {left}, top: {top}, behavior: 'instant' }}); \
         return {{ status: 'done', before, after: {{ x: scrollX, y: scrollY }} }}; }})()"
    )
}

/// Scrolls the first element matching `selector` into view and returns the
/// viewport point a click should use, once checked that a click there
/// reaches the element: `{status: 'ok', x, y}`, or `missing`, `hidden` (no
/// visible box), `outside` (not in the viewport even after scrolling) or
/// `covered` (with `by`, the element on top, and `x`, `y`). A pointer click
/// lands on whatever is at the point, so without the check an overlay, or
/// the page corner for a hidden element, would get it.
pub fn click_point(selector: &str) -> String {
    on_element(selector, "{ status: 'missing' }", CLICK_POINT)
}

const CLICK_POINT: &str = "el.scrollIntoView({ behavior: 'instant', block: 'center', inline: 'center' }); \
    const box = Array.from(el.getClientRects()).find(r => r.width > 0 && r.height > 0); \
    if (!box || getComputedStyle(el).visibility !== 'visible') return { status: 'hidden' }; \
    const left = Math.max(box.left, 0), right = Math.min(box.right, innerWidth); \
    const top = Math.max(box.top, 0), bottom = Math.min(box.bottom, innerHeight); \
    if (right <= left || bottom <= top) return { status: 'outside' }; \
    const x = Math.round((left + right) / 2), y = Math.round((top + bottom) / 2); \
    const hit = document.elementFromPoint(x, y); \
    if (hit && hit !== el && !el.contains(hit)) { \
        const kind = typeof hit.className === 'string' && hit.className.trim() ? '.' + hit.className.trim().split(/\\s+/)[0] : ''; \
        return { status: 'covered', by: hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') + kind, x, y }; \
    } \
    return { status: 'ok', x, y };";

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

/// Most characters of HTML `inspect_dom` returns without a query: the
/// whole document of a large page would flood the agent, and a reply over
/// 16 MB closes the browser connection.
pub const DOM_MAX_CHARS: usize = 100_000;

/// Most characters of context on each side of an `inspect_dom` match.
pub const DOM_MAX_CONTEXT: usize = 10_000;

/// `inspect_dom`'s search and cut, run in the page so only the result
/// crosses the connection: `{total, html}` without a query (at most
/// [`DOM_MAX_CHARS`]), else `{total, found, position, context}` with
/// positions in characters.
pub fn dom_query(query: Option<&str>, before: usize, after: usize) -> String {
    let query = query.map_or_else(|| "null".to_string(), string_literal);
    let (before, after) = (before.min(DOM_MAX_CONTEXT), after.min(DOM_MAX_CONTEXT));
    format!(
        "(() => {{ const html = document.documentElement.outerHTML; const query = {query}; \
         if (query === null) return {{ total: html.length, html: html.slice(0, {DOM_MAX_CHARS}) }}; \
         const position = html.indexOf(query); \
         if (position < 0) return {{ total: html.length, found: false }}; \
         const context = html.slice(Math.max(0, position - {before}), position + query.length + {after}); \
         return {{ total: html.length, found: true, position, context }}; }})()"
    )
}

/// The document's scrolling size in CSS pixels, `{width, height}` (what a
/// full-page capture covers).
pub const DOCUMENT_SIZE: &str = "(() => { const root = document.documentElement, body = document.body; \
     return { width: Math.max(root.scrollWidth, body ? body.scrollWidth : 0), \
     height: Math.max(root.scrollHeight, body ? body.scrollHeight : 0) }; })()";

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
            click_point(selector),
            prepare_fill(selector, selector),
            fill_result(selector, selector),
        ] {
            // Then it is one JSON string literal that parses back to the original
            assert_eq!(embedded_selector(&expression), selector, "{expression}");
        }
    }

    /// AUD-IN-27: inspect_dom searches and cuts in the page; the query
    /// reaches JavaScript as one string literal.
    #[test]
    fn dom_query_cuts_in_the_page() {
        let query = r#"a"); window.__pwned = 1; ("#;
        let expression = dom_query(Some(query), 10, 20);
        assert!(expression.contains("outerHTML"), "{expression}");
        assert!(expression.contains(&string_literal(query)), "{expression}");
        assert!(
            expression.contains(&DOM_MAX_CHARS.to_string()),
            "{expression}"
        );
        assert!(dom_query(None, 1, 1).contains("const query = null;"));
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
