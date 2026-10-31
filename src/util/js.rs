//! JavaScript snippets the tools evaluate in the page, shared by every
//! backend. Values from the agent reach JavaScript only as JSON string
//! literals, never by hand-escaping (a `\` used to break out of the string).

/// `value` as a JavaScript string literal (JSON strings are valid JS).
pub fn string_literal(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// The separator of a selector that descends into an open shadow root or a
/// same-origin iframe: `host >>> inner`, chainable.
pub const PIERCE: &str = ">>>";

/// Whether `selector` is a ref from `snapshot` (`e12`) rather than CSS (no
/// HTML element is named like that, and custom elements need a hyphen).
pub fn is_ref(selector: &str) -> bool {
    let selector = selector.trim();
    selector.len() > 1
        && selector.starts_with('e')
        && selector[1..].bytes().all(|b| b.is_ascii_digit())
}

/// The hint an error about a missing element adds for a ref.
pub fn ref_hint(selector: &str) -> &'static str {
    if is_ref(selector) {
        " (a ref from 'snapshot' lasts until the page navigates or its element is removed: take a new snapshot)"
    } else {
        ""
    }
}

/// Declares `resolve(target)`: the first element `target` names, or `null`.
/// A ref from `snapshot` (`e12`) names its element while it is in the page
/// (the map `util::outline` fills, in the top window: refs die with its
/// document).
/// Otherwise each `>>>`-separated part is a CSS selector matched inside
/// what the previous part matched (its open shadow root, or its document
/// when it is a same-origin iframe); anything else there throws, naming it.
pub(crate) const RESOLVE: &str = "const resolve = target => { \
        if (/^\\s*e\\d+\\s*$/.test(target)) { \
            const refs = window[Symbol.for('browser-control-mcp.refs')]; \
            const weak = refs && refs.get(target.trim()), found = weak && weak.deref(); \
            return found && found.isConnected ? found : null; \
        } \
        const parts = target.split('>>>').map(part => part.trim()); \
        let scope = document, el = null; \
        for (const [i, part] of parts.entries()) { \
            if (i > 0) { \
                if (el.shadowRoot) scope = el.shadowRoot; \
                else if (el.contentDocument) scope = el.contentDocument; \
                else throw new Error(`'${parts[i - 1]}' is neither an open shadow host nor a same-origin iframe, so '>>>' cannot descend into it`); \
            } \
            el = scope.querySelector(part); \
            if (!el) return null; \
        } \
        return el; \
    };";

/// `(function() { const el = <selector>; if (!el) return <missing>; <body> })()`:
/// the one shape every element snippet has (the selector may pierce, see
/// [`RESOLVE`]).
fn on_element(selector: &str, missing: &str, body: &str) -> String {
    format!(
        "(function() {{ {RESOLVE} const el = resolve({}); if (!el) return {missing}; {body} }})()",
        string_literal(selector)
    )
}

/// An element snippet answering `{ status: 'missing' }` when `selector`
/// names nothing (the shape of every status snippet).
fn status_of(selector: &str, body: &str) -> String {
    on_element(selector, "{ status: 'missing' }", body)
}

/// Scrolls the first element matching `selector` into view. Returns
/// `{status: 'missing'}`, `{status: 'hidden'}` (no layout box, nothing to
/// scroll to) or `{status: 'done', before, after, in_view}` with the window
/// positions, so the reply can tell what moved.
pub fn scroll_into_view(selector: &str) -> String {
    status_of(
        selector,
        "const before = { x: scrollX, y: scrollY }; \
         el.scrollIntoView({ behavior: 'instant', block: 'center' }); \
         const r = el.getBoundingClientRect(), win = el.ownerDocument.defaultView; \
         if (r.width === 0 && r.height === 0) return { status: 'hidden' }; \
         const in_view = r.bottom > 0 && r.top < win.innerHeight && r.right > 0 && r.left < win.innerWidth; \
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
/// the page corner for a hidden element, would get it. An element inside
/// iframes is checked in its own document, then each frame in its parent's,
/// and the point is the top window's.
pub fn click_point(selector: &str) -> String {
    status_of(selector, CLICK_POINT)
}

const CLICK_POINT: &str = "el.scrollIntoView({ behavior: 'instant', block: 'center', inline: 'center' }); \
    let win = el.ownerDocument.defaultView; \
    const box = Array.from(el.getClientRects()).find(r => r.width > 0 && r.height > 0); \
    if (!box || win.getComputedStyle(el).visibility !== 'visible') return { status: 'hidden' }; \
    const left = Math.max(box.left, 0), right = Math.min(box.right, win.innerWidth); \
    const top = Math.max(box.top, 0), bottom = Math.min(box.bottom, win.innerHeight); \
    if (right <= left || bottom <= top) return { status: 'outside' }; \
    let x = Math.round((left + right) / 2), y = Math.round((top + bottom) / 2); \
    const covered = hit => { \
        const kind = typeof hit.className === 'string' && hit.className.trim() ? '.' + hit.className.trim().split(/\\s+/)[0] : ''; \
        return { status: 'covered', by: hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') + kind, x, y }; \
    }; \
    const hit = el.getRootNode().elementFromPoint(x, y); \
    if (hit && hit !== el && !el.contains(hit)) return covered(hit); \
    while (win.frameElement) { \
        const frame = win.frameElement, r = frame.getBoundingClientRect(); \
        x = Math.round(x + r.left + frame.clientLeft); y = Math.round(y + r.top + frame.clientTop); \
        win = frame.ownerDocument.defaultView; \
        if (x < 0 || y < 0 || x >= win.innerWidth || y >= win.innerHeight) return { status: 'outside' }; \
        const over = frame.getRootNode().elementFromPoint(x, y); \
        if (over && over !== frame) return covered(over); \
    } \
    return { status: 'ok', x, y };";

/// What `fill_form` does with the element `selector` names:
/// `{status: 'missing'}`, `{status: 'box', checked}` for a checkbox or
/// radio (it is clicked when its state differs), or `{status: 'field'}`
/// (it is filled like `fill_input`).
pub fn form_field(selector: &str) -> String {
    status_of(
        selector,
        "const W = el.ownerDocument.defaultView; \
         const box = el instanceof W.HTMLInputElement && (el.type === 'checkbox' || el.type === 'radio'); \
         return box ? { status: 'box', checked: el.checked } : { status: 'field' };",
    )
}

/// Most items `web_storage` lists.
pub const STORAGE_MAX_ITEMS: usize = 200;

/// Most characters of a value `web_storage` returns.
pub const STORAGE_MAX_VALUE: usize = 2000;

/// `web_storage`'s `action` (`get`, `set`, `delete`, `clear`) on the page's
/// `localStorage` or `sessionStorage` (`kind`), for `key` and `value`:
/// `{origin, items: [[key, value, length]], total}` for `get` (values cut
/// to [`STORAGE_MAX_VALUE`]), `{origin, had}` for `delete`,
/// `{origin, cleared}` for `clear`, `{origin}` for `set`. A page without
/// storage (an opaque origin) throws.
pub fn web_storage(action: &str, kind: &str, key: Option<&str>, value: Option<&str>) -> String {
    let literal = |v: Option<&str>| v.map_or_else(|| "null".to_string(), string_literal);
    format!(
        "(() => {{ const action = {}, key = {}, value = {}; \
         const store = {kind} === 'session' ? sessionStorage : localStorage, origin = location.origin; \
         const item = k => {{ const v = store.getItem(k); return [k, v.slice(0, {STORAGE_MAX_VALUE}), v.length]; }}; \
         if (action === 'get') {{ \
           if (key !== null) return {{ origin, total: store.getItem(key) === null ? 0 : 1, items: store.getItem(key) === null ? [] : [item(key)] }}; \
           const items = []; for (let i = 0; i < store.length && i < {STORAGE_MAX_ITEMS}; i++) items.push(item(store.key(i))); \
           return {{ origin, total: store.length, items }}; \
         }} \
         if (action === 'set') {{ store.setItem(key, value); return {{ origin }}; }} \
         if (action === 'delete') {{ const had = store.getItem(key) !== null; store.removeItem(key); return {{ origin, had }}; }} \
         const cleared = store.length; store.clear(); return {{ origin, cleared }}; }})()",
        string_literal(action),
        literal(key),
        literal(value),
        kind = string_literal(kind),
    )
}

/// Focuses the element `selector` names (`press_key`): `{status}`
/// `missing`, `focused` (it, or an element inside it, has the focus) or
/// `unfocusable`.
pub fn focus(selector: &str) -> String {
    status_of(
        selector,
        "el.focus(); const active = el.getRootNode().activeElement; \
         return { status: active && (active === el || el.contains(active)) ? 'focused' : 'unfocusable' };",
    )
}

/// Whether the element `selector` names can take files (`upload_file`):
/// `{status}` `missing`, `not_file` (not an `<input type=file>`), `disabled`
/// or `ok` (with `multiple`).
pub fn file_input(selector: &str) -> String {
    status_of(
        selector,
        "const W = el.ownerDocument.defaultView; \
         if (!(el instanceof W.HTMLInputElement) || el.type !== 'file') return { status: 'not_file' }; \
         if (el.disabled) return { status: 'disabled' }; \
         return { status: 'ok', multiple: el.multiple };",
    )
}

/// The element `selector` names itself (`null` when none), for commands
/// that take an element reference rather than a value.
pub fn element(selector: &str) -> String {
    on_element(selector, "null", "return el;")
}

/// The names of the files the `<input type=file>` `selector` names holds.
pub fn file_names(selector: &str) -> String {
    on_element(
        selector,
        "[]",
        "return Array.from(el.files || []).map(f => f.name);",
    )
}

/// The property where the preparation keeps the value it found, for the
/// check to fire `change` (a symbol: it never collides with the page's).
const FILL_BEFORE: &str = "Symbol.for('browser-control-mcp.fill-before')";

/// Readies the first element matching `selector` for `text` (`fill_input`).
/// A text field is focused and emptied (through the native `value` setter,
/// which frameworks such as React observe); a contenteditable element is
/// focused and its contents selected, so the first typed key replaces them
/// through `beforeinput` (rich text editors keep their own model, which
/// removing their DOM nodes would leave behind), and an empty `text`
/// deletes them through the editing commands; a `<select>` gets the option
/// whose value or label is `text`. Returns `{status}`: `missing`,
/// `not_editable` (with `reason`), `no_option` (with `options`), `selected`
/// or `ready`. Checking focus matters: typing goes to whatever element has
/// it, so an element that cannot take it would send the text elsewhere.
pub fn prepare_fill(selector: &str, text: &str) -> String {
    status_of(
        selector,
        &format!("const text = {}; {PREPARE_FILL}", string_literal(text)),
    )
}

const PREPARE_FILL: &str = "const W = el.ownerDocument.defaultView; \
    if (el instanceof W.HTMLSelectElement) { \
        if (el.matches(':disabled')) return { status: 'not_editable', reason: 'disabled' }; \
        const options = Array.from(el.options); \
        const option = options.find(o => o.value === text) || options.find(o => o.label.trim() === text.trim()); \
        if (!option) return { status: 'no_option', options: options.slice(0, 20).map(o => o.label.trim()) }; \
        option.selected = true; \
        el.dispatchEvent(new Event('input', { bubbles: true })); \
        el.dispatchEvent(new Event('change', { bubbles: true })); \
        return { status: 'selected' }; \
    } \
    const field = el instanceof W.HTMLInputElement || el instanceof W.HTMLTextAreaElement; \
    const typed = ['text', 'search', 'email', 'url', 'tel', 'password', 'number', 'date', 'datetime-local', 'month', 'week', 'time']; \
    if (el instanceof W.HTMLInputElement && !typed.includes(el.type)) return { status: 'not_editable', reason: 'an input of type ' + el.type }; \
    if (!field && !el.isContentEditable) return { status: 'not_editable', reason: 'not an input, textarea or contenteditable element' }; \
    if (field && el.matches(':disabled')) return { status: 'not_editable', reason: 'disabled' }; \
    if (field && el.readOnly) return { status: 'not_editable', reason: 'read-only' }; \
    el.focus(); \
    const active = el.getRootNode().activeElement; \
    if (active !== el && !(el.isContentEditable && active && active.contains(el))) return { status: 'not_editable', reason: 'it cannot take focus (hidden or inert)' }; \
    if (field) { \
        el[Symbol.for('browser-control-mcp.fill-before')] = el.value; \
        const value = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value'); \
        if (value && value.set) { value.set.call(el, ''); } else { el.value = ''; } \
        el.dispatchEvent(new Event('input', { bubbles: true })); \
    } else { \
        const doc = el.ownerDocument, range = doc.createRange(), selection = doc.getSelection(); \
        range.selectNodeContents(el); selection.removeAllRanges(); selection.addRange(range); \
        if (text === '' && el.textContent !== '') doc.execCommand('delete'); \
    } \
    return { status: 'ready' };";

/// What the element matching `selector` holds after `text` was typed:
/// `{status: 'missing'}` or `{status: 'done', matches, empty}` (the value
/// itself never leaves the page, as it may be a password). `change` is the
/// browser's, when the field loses focus, as for a user; but a window
/// without focus (Firefox headless, a background window) fires no blur, so
/// there an input or textarea whose value differs from the one the
/// preparation found gets `change` here.
pub fn fill_result(selector: &str, text: &str) -> String {
    status_of(
        selector,
        &format!(
            "const text = {}; \
             const W = el.ownerDocument.defaultView; \
             const field = el instanceof W.HTMLInputElement || el instanceof W.HTMLTextAreaElement; \
             const value = field ? el.value : el.textContent.replace(/\\uFEFF/g, '').replace(/\\u00A0/g, ' '); \
             if (field) {{ const before = el[{FILL_BEFORE}]; delete el[{FILL_BEFORE}]; \
               if (!el.ownerDocument.hasFocus() && value !== before) el.dispatchEvent(new Event('change', {{ bubbles: true }})); }} \
             return {{ status: 'done', matches: value === text, empty: value === '' }};",
            string_literal(text)
        ),
    )
}

/// Most characters of HTML `inspect_dom` returns without a query: the
/// whole document of a large page would flood the agent, and a reply over
/// 256 MB closes the browser connection.
pub const DOM_MAX_CHARS: usize = 100_000;

/// Most characters of context on each side of an `inspect_dom` match.
pub const DOM_MAX_CONTEXT: usize = 10_000;

/// Most iframes `inspect_dom` reads into the document (and how deep).
const DOM_MAX_FRAMES: usize = 20;
const DOM_MAX_FRAME_DEPTH: usize = 3;

/// Declares `serialize(doc, depth)` and `state`: the HTML of `doc`'s root
/// with its open shadow roots (`getHTML` with `shadowRoots`, written as
/// `<template shadowrootmode>`; `outerHTML` left them out), followed by
/// each same-origin iframe's document after an `<!-- iframe src: -->`
/// comment. `state.skipped` counts the iframes another origin keeps out.
fn serialize_document() -> String {
    format!(
        "const state = {{ frames: 0, skipped: 0 }}; \
         const serialize = (doc, depth) => {{ \
           const root = doc.documentElement; \
           const roots = []; \
           const walk = node => {{ for (const e of node.querySelectorAll('*')) if (e.shadowRoot) {{ roots.push(e.shadowRoot); walk(e.shadowRoot); }} }}; \
           walk(doc); \
           let html = root.outerHTML; \
           if (typeof root.getHTML === 'function') {{ \
             const shell = root.cloneNode(false).outerHTML, close = shell.lastIndexOf('</'); \
             html = shell.slice(0, close) + root.getHTML({{ shadowRoots: roots }}) + shell.slice(close); \
           }} \
           const frames = [doc, ...roots].flatMap(n => Array.from(n.querySelectorAll('iframe, frame'))); \
           for (const frame of frames) {{ \
             let inner = null; \
             try {{ inner = frame.contentDocument; }} catch (e) {{}} \
             if (!inner || !inner.documentElement) {{ state.skipped++; continue; }} \
             if (depth >= {DOM_MAX_FRAME_DEPTH} || state.frames >= {DOM_MAX_FRAMES}) continue; \
             state.frames++; \
             html += '\\n<!-- iframe ' + (frame.getAttribute('src') || 'about:blank') + ': -->\\n' + serialize(inner, depth + 1); \
           }} \
           return html; \
         }};"
    )
}

/// `inspect_dom`'s search and cut, run in the page so only the result
/// crosses the connection: `{total, html}` without a query (at most
/// [`DOM_MAX_CHARS`]), else `{total, found, position, context}` with
/// positions in characters, both with `skipped_frames` (iframes of
/// another origin, which cannot be read). The text searched is the top
/// document with its open shadow roots, then each same-origin iframe's.
pub fn dom_query(query: Option<&str>, before: usize, after: usize) -> String {
    let query = query.map_or_else(|| "null".to_string(), string_literal);
    let (before, after) = (before.min(DOM_MAX_CONTEXT), after.min(DOM_MAX_CONTEXT));
    let serialize = serialize_document();
    format!(
        "(() => {{ {serialize} const html = serialize(document, 0); const query = {query}; \
         const skipped_frames = state.skipped; \
         if (query === null) return {{ total: html.length, html: html.slice(0, {DOM_MAX_CHARS}), skipped_frames }}; \
         const position = html.indexOf(query); \
         if (position < 0) return {{ total: html.length, found: false, skipped_frames }}; \
         const context = html.slice(Math.max(0, position - {before}), position + query.length + {after}); \
         return {{ total: html.length, found: true, position, context, skipped_frames }}; }})()"
    )
}

/// What `wait_for` checks; a `None` is not checked.
pub struct WaitConditions<'a> {
    /// Text the page's visible text must contain.
    pub text: Option<&'a str>,
    /// Text the page's visible text must no longer contain.
    pub text_gone: Option<&'a str>,
    /// CSS selector whose first match must be in `state`.
    pub selector: Option<&'a str>,
    /// `visible`, `hidden`, `attached` or `detached`.
    pub state: &'a str,
    /// Text the tab's URL must contain.
    pub url: Option<&'a str>,
}

/// One check of `wait_for`'s conditions, so only booleans cross the
/// connection: `{href, text?, text_gone?, selector?, url?}`, each key
/// present when checked and `true` when its condition holds. The text is
/// the visible text (`innerText`) of the top document, its open shadow
/// roots and its same-origin iframes, as `inspect_dom` reads them; the
/// selector is matched in the top document, as the input tools do.
pub fn wait_check(wanted: &WaitConditions) -> String {
    let literal = |value: Option<&str>| value.map_or_else(|| "null".to_string(), string_literal);
    format!(
        "(() => {{ const text = {}, gone = {}, selector = {}, state = {}, url = {}; {RESOLVE} {WAIT_CHECK} }})()",
        literal(wanted.text),
        literal(wanted.text_gone),
        literal(wanted.selector),
        string_literal(wanted.state),
        literal(wanted.url),
    )
}

const WAIT_CHECK: &str = "const texts = []; \
    const read = (doc, depth) => { \
        const roots = []; \
        const walk = node => { for (const e of node.querySelectorAll('*')) if (e.shadowRoot) { roots.push(e.shadowRoot); walk(e.shadowRoot); } }; \
        walk(doc); \
        if (doc.body) texts.push(doc.body.innerText); \
        for (const root of roots) for (const child of root.children) \
            if (!['STYLE', 'SCRIPT', 'TEMPLATE'].includes(child.tagName)) texts.push(child.innerText); \
        if (depth >= 3) return; \
        for (const frame of [doc, ...roots].flatMap(n => Array.from(n.querySelectorAll('iframe, frame')))) { \
            let inner = null; try { inner = frame.contentDocument; } catch (e) {} \
            if (inner && inner.documentElement) read(inner, depth + 1); \
        } \
    }; \
    if (text !== null || gone !== null) read(document, 0); \
    const all = texts.join('\\n'); \
    const out = { href: location.href }; \
    if (text !== null) out.text = all.includes(text); \
    if (gone !== null) out.text_gone = !all.includes(gone); \
    if (selector !== null) { \
        const el = resolve(selector); \
        const shown = !!el && Array.from(el.getClientRects()).some(r => r.width > 0 && r.height > 0) \
            && el.ownerDocument.defaultView.getComputedStyle(el).visibility === 'visible'; \
        out.selector = { attached: !!el, detached: !el, visible: shown, hidden: !shown }[state]; \
    } \
    if (url !== null) out.url = location.href.includes(url); \
    return out;";

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

    /// The argument of `resolve(..)` in `expression`, parsed back as JSON.
    fn embedded_selector(expression: &str) -> String {
        let call = "const el = resolve(";
        let start = expression.find(call).unwrap() + call.len();
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

    /// AUD-IN-24: the serialized document includes open shadow roots
    /// (`getHTML` with `shadowRoots`) and same-origin iframes, and counts
    /// the iframes it cannot read.
    #[test]
    fn dom_query_includes_shadow_roots_and_frames() {
        let expression = dom_query(None, 1, 1);
        assert!(
            expression.contains("getHTML({ shadowRoots: roots })"),
            "{expression}"
        );
        assert!(expression.contains("contentDocument"), "{expression}");
        assert!(expression.contains("<!-- iframe "), "{expression}");
        assert!(expression.contains("skipped_frames"), "{expression}");
    }

    /// AUD-IN-11: `change` comes from the browser when the field loses
    /// focus; in a window without focus (no blur ever comes: Firefox
    /// headless) the check fires it for a field whose value changed.
    #[test]
    fn fill_fires_change_when_the_value_changed() {
        let prepare = prepare_fill("#q", "x");
        assert!(
            prepare.contains(&format!("el[{FILL_BEFORE}] = ")),
            "{prepare}"
        );
        let check = fill_result("#q", "x");
        assert!(check.contains(&format!("el[{FILL_BEFORE}]")), "{check}");
        assert!(
            check.contains("new Event('change', { bubbles: true })"),
            "{check}"
        );
        assert!(
            check.contains("!el.ownerDocument.hasFocus() && value !== before"),
            "{check}"
        );
    }

    /// ISSUE-3: every element snippet resolves its selector through the
    /// piercing resolver: `>>>` descends into an open shadow root or a
    /// same-origin iframe's document, and anything else throws naming it.
    #[test]
    fn element_snippets_pierce_shadow_roots_and_frames() {
        // Given the snippets of every element tool
        let wanted = WaitConditions {
            text: None,
            text_gone: None,
            selector: Some("#fr >>> #b"),
            state: "visible",
            url: None,
        };
        for expression in [
            scroll_into_view("#a"),
            click_point("#a"),
            prepare_fill("#a", "x"),
            fill_result("#a", "x"),
            wait_check(&wanted),
        ] {
            // Then each declares the resolver and none queries the document directly
            assert!(expression.contains(RESOLVE), "{expression}");
            assert!(
                !expression.contains("document.querySelector("),
                "{expression}"
            );
        }
        // And fields are recognised by their own window's classes (an
        // iframe's input is not an instance of the top window's)
        for expression in [prepare_fill("#a", "x"), fill_result("#a", "x")] {
            assert!(expression.contains("const W = el.ownerDocument.defaultView;"));
            assert!(!expression.contains("instanceof HTML"), "{expression}");
        }
        assert!(RESOLVE.contains("target.split('>>>')"));
        assert!(RESOLVE.contains("el.shadowRoot") && RESOLVE.contains("el.contentDocument"));
        assert!(RESOLVE.contains("is neither an open shadow host nor a same-origin iframe"));
        assert_eq!(PIERCE, ">>>");
    }

    /// ISSUE-4: a ref from `snapshot` is told from CSS and resolves through
    /// the page's ref map, the one `snapshot` fills.
    #[test]
    fn refs_resolve_through_the_snapshot_map() {
        for selector in ["e1", "e12", " e305 "] {
            assert!(is_ref(selector), "{selector}");
        }
        for selector in ["e", "em", "#e12", "e12 > a", "e1-2", "button"] {
            assert!(!is_ref(selector), "{selector}");
        }
        assert!(RESOLVE.contains("found.isConnected"));
        assert!(ref_hint("e3").contains("take a new snapshot"));
        assert_eq!(ref_hint("#e3"), "");
    }

    /// ISSUE-3: a click inside iframes is checked in each document up to
    /// the top window, whose coordinates it returns.
    #[test]
    fn click_point_climbs_out_of_frames() {
        let expression = click_point("#fr >>> #b");
        assert!(
            expression.contains("while (win.frameElement)"),
            "{expression}"
        );
        assert!(expression.contains("frame.clientLeft"), "{expression}");
        assert!(
            expression.contains("el.getRootNode().elementFromPoint"),
            "{expression}"
        );
        assert!(expression.contains("over !== frame"), "{expression}");
    }

    /// ISSUE-2: wait_for's values reach JavaScript as JSON literals (`null`
    /// when not checked), and its text covers shadow roots and iframes.
    #[test]
    fn wait_check_embeds_values_as_literals() {
        // Given a text crafted to close the string, and no selector
        let text = r#"x", gone = 1; window.__pwned = 1; ""#;
        let wanted = WaitConditions {
            text: Some(text),
            text_gone: None,
            selector: None,
            state: "visible",
            url: Some("/home"),
        };
        // When the check is built
        let expression = wait_check(&wanted);
        // Then the text parses back whole and the rest are literals
        assert_eq!(embedded_text(&expression), text);
        assert!(
            expression
                .contains(", gone = null, selector = null, state = \"visible\", url = \"/home\";"),
            "{expression}"
        );
        assert!(expression.contains("e.shadowRoot"), "{expression}");
        assert!(expression.contains("contentDocument"), "{expression}");
    }

    /// The `const text = <literal>;` of a fill snippet, parsed back as JSON.
    fn embedded_text(expression: &str) -> String {
        let start = expression.find("const text = ").unwrap() + "const text = ".len();
        let mut values = serde_json::Deserializer::from_str(&expression[start..]).into_iter();
        values.next().unwrap().unwrap()
    }

    /// FIX-07 / AUD-IN-08: fill_input empties the field it focused, and
    /// the text reaches JavaScript as one string literal.
    /// ISSUE-23: a contenteditable element is not emptied through its DOM
    /// (rich text editors keep their own model of it): its contents are
    /// selected so the first typed key replaces them, and an empty text
    /// deletes them through the editing commands; the result is read
    /// without the editors' zero-width placeholders.
    #[test]
    fn rich_text_editors_keep_their_model() {
        let prepare = prepare_fill("#rich", "x");
        assert!(!prepare.contains("el.textContent = ''"), "{prepare}");
        assert!(prepare.contains("range.selectNodeContents(el)"), "{prepare}");
        assert!(prepare.contains("execCommand('delete')"), "{prepare}");
        let result = fill_result("#rich", "x");
        assert!(result.contains("\\uFEFF"), "{result}");
    }

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
            expression.contains("el.getRootNode().activeElement"),
            "{expression}"
        );
        assert_eq!(embedded_text(&expression), text);
        assert_eq!(embedded_text(&fill_result("#q", text)), text);
    }
}
