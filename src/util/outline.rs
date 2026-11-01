//! The page outline behind `snapshot` and `find_in_snapshot`: the
//! elements an agent acts on, with an approximate accessibility role,
//! accessible name, states and a ref (`e12`) that every selector argument
//! accepts. It is computed in the page with the same JavaScript on every
//! browser, so both answer the same outline and refs resolve the same way
//! (`js::RESOLVE`).
//!
//! One line per element with a role, indented under its nearest such
//! ancestor, following the rendered tree: open shadow roots, slots and
//! same-origin iframes included, hidden elements left out. Elements without
//! a role are transparent; their text becomes `text:` lines. Layout tables
//! (no headers, no role) are transparent too, and a cell is named by its
//! content only when that content is plain text, folded into the name, so
//! no text is printed twice. A `clickable` (a handler or a pointer cursor,
//! no role) that holds interactive elements lists them instead of being a
//! leaf named from its whole text.

use super::js::{RESOLVE, string_literal};

/// Most characters `snapshot` returns: a large page would flood the agent
/// (`find_in_snapshot` searches the whole outline instead).
pub const SNAPSHOT_MAX_CHARS: usize = 50_000;

/// Most characters of outline `find_in_snapshot` searches (in the page).
pub const FIND_MAX_CHARS: usize = 2_000_000;

/// Most matches `find_in_snapshot` shows.
pub const FIND_MAX_MATCHES: usize = 20;

/// Deepest level walked when no `depth` is given.
const MAX_DEPTH: u32 = 1000;

/// Declares `outline(root, maxDepth, maxChars)`: `{lines, truncated,
/// foreign}` for `root` (the document, or an element and its subtree).
/// Elements with a role get a ref, kept in the top window's map (the one
/// `js::RESOLVE` reads); an element keeps its ref across outlines while it
/// is in the page, and entries of removed elements are dropped. `foreign`
/// lists the refs of the iframes of another origin it could not read, so
/// the server can find each frame and outline it there (`handler::frames`).
const OUTLINE: &str = r#"const outline = (root, maxDepth, maxChars) => {
    const refsKey = Symbol.for('browser-control-mcp.refs'), refKey = Symbol.for('browser-control-mcp.ref');
    const countKey = Symbol.for('browser-control-mcp.ref-count');
    const refs = window[refsKey] || (window[refsKey] = new Map());
    for (const [ref, weak] of refs) { const known = weak.deref(); if (!known || !known.isConnected) refs.delete(ref); }
    const refOf = el => {
        const known = el[refKey];
        if (known && refs.has(known) && refs.get(known).deref() === el) return known;
        window[countKey] = (window[countKey] || 0) + 1;
        const ref = 'e' + window[countKey];
        el[refKey] = ref; refs.set(ref, new WeakRef(el));
        return ref;
    };
    const clean = text => (text || '').replace(/\s+/g, ' ').trim();
    const cut = (text, max) => text.length > max ? text.slice(0, max - 1) + '…' : text;
    const lines = [], foreign = [];
    let size = 0, truncated = false, pending = '', pendingDepth = 0;
    const doc = root.nodeType === 9 ? root : root.ownerDocument;
    let modal = null; try { modal = doc.querySelector('dialog:modal'); } catch (e) {}
    const outsideModal = node => !!modal && node.ownerDocument === doc && !modal.contains(node) && !(node.nodeType === 1 && node.contains(modal));
    const push = (depth, text) => {
        if (truncated) return;
        const line = '  '.repeat(depth) + '- ' + text;
        if (size + line.length + 1 > maxChars) { truncated = true; return; }
        size += line.length + 1; lines.push(line);
    };
    const flush = () => { if (pending) push(pendingDepth, 'text: ' + cut(pending, 300)); pending = ''; };
    const addText = (text, depth) => {
        const words = clean(text); if (!words) return;
        if (pending && depth !== pendingDepth) flush();
        pending = pending ? pending + ' ' + words : words; pendingDepth = depth;
    };
    const INPUTS = { button: 'button', submit: 'button', reset: 'button', image: 'button', file: 'button',
        checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton', search: 'searchbox' };
    const TAGS = { BUTTON: 'button', SUMMARY: 'button', TEXTAREA: 'textbox', H1: 'heading', H2: 'heading',
        H3: 'heading', H4: 'heading', H5: 'heading', H6: 'heading', NAV: 'navigation', MAIN: 'main',
        FORM: 'form', DIALOG: 'dialog', PROGRESS: 'progressbar', METER: 'meter', UL: 'list', OL: 'list', LI: 'listitem', TABLE: 'table', TR: 'row',
        TD: 'cell', TH: 'columnheader', OPTION: 'option', IFRAME: 'iframe', FRAME: 'iframe',
        HEADER: 'banner', FOOTER: 'contentinfo', ASIDE: 'complementary' };
    const firstRole = el => (el.getAttribute('role') || '').trim().split(/\s+/)[0];
    const layoutTables = new Map();
    const layoutTable = table => {
        if (!table) return false;
        if (!layoutTables.has(table)) {
            const role = firstRole(table);
            const headed = table.caption || table.tHead || table.hasAttribute('summary')
                || Array.from(table.rows).some(row => Array.from(row.cells).some(cell => cell.tagName === 'TH'));
            layoutTables.set(table, role === 'presentation' || role === 'none' || (!role && !headed));
        }
        return layoutTables.get(table);
    };
    const roleOf = el => {
        const explicit = firstRole(el);
        if (explicit === 'presentation' || explicit === 'none') return null;
        if (explicit) return explicit;
        const tag = el.tagName;
        if ((tag === 'TABLE' || tag === 'TR' || tag === 'TD') && layoutTable(tag === 'TABLE' ? el : el.closest('table'))) return null;
        if (tag === 'A' || tag === 'AREA') return el.hasAttribute('href') ? 'link' : null;
        if (tag === 'INPUT') return el.type === 'hidden' ? null : INPUTS[el.type] || 'textbox';
        if (tag === 'SELECT') return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
        if (tag === 'IMG') return el.getAttribute('alt') === '' ? null : 'img';
        if (TAGS[tag]) return TAGS[tag];
        if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
        return el.hasAttribute('onclick') || el.hasAttribute('tabindex') && el.tabIndex >= 0 ? 'clickable' : null;
    };
    const LEAF = new Set(['button', 'link', 'heading', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio',
        'img', 'slider', 'spinbutton', 'option', 'switch', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'clickable',
        'progressbar', 'meter']);
    const FROM_CONTENT = new Set(['button', 'link', 'heading', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
        'option', 'switch', 'clickable', 'checkbox', 'radio']);
    const NAMED_BY_TEXT = new Set(['cell', 'columnheader', 'rowheader', 'gridcell']);
    const foldText = (at, depth, role) => {
        const own = lines.slice(at + 1), marker = '  '.repeat(depth + 1) + '- text: ';
        if (truncated || !own.length || !own.every(line => line.startsWith(marker))) return;
        const folded = own.map(line => line.slice(marker.length)).join(' ');
        if (folded.length > 100) return;
        const head = '  '.repeat(depth) + '- ' + role, line = head + ' ' + JSON.stringify(folded) + lines[at].slice(head.length);
        size += line.length - lines[at].length - own.reduce((n, l) => n + l.length + 1, 0);
        lines.splice(at, lines.length - at, line);
    };
    const INTERACTIVE = 'a[href], button, input, select, textarea, summary, [role], [onclick], [tabindex], [contenteditable]';
    const textOf = el => clean(el.innerText !== undefined ? el.innerText : el.textContent);
    const nameOf = (el, role) => {
        const root = el.getRootNode();
        const by = el.getAttribute('aria-labelledby');
        if (by && root.getElementById) {
            const named = clean(by.split(/\s+/).map(id => root.getElementById(id)).filter(Boolean).map(textOf).join(' '));
            if (named) return named;
        }
        const label = clean(el.getAttribute('aria-label')); if (label) return label;
        if (el.labels && el.labels.length) { const named = clean(Array.from(el.labels).map(textOf).join(' ')); if (named) return named; }
        if (el.tagName === 'IMG' || el.tagName === 'AREA' || (el.tagName === 'INPUT' && el.type === 'image')) return clean(el.getAttribute('alt'));
        if (el.tagName === 'INPUT' && ['button', 'submit', 'reset'].includes(el.type)) return clean(el.value) || el.type;
        if (FROM_CONTENT.has(role)) { const named = textOf(el); if (named) return named; }
        return clean(el.getAttribute('title') || el.getAttribute('placeholder'));
    };
    const details = (el, role) => {
        const out = [], flag = (on, name) => { if (on) out.push('[' + name + ']'); };
        const box = el.type === 'checkbox' || el.type === 'radio';
        flag(box ? el.checked : el.getAttribute('aria-checked') === 'true', 'checked');
        flag(el.disabled || el.getAttribute('aria-disabled') === 'true', 'disabled');
        flag(el.required, 'required');
        flag((role === 'option' && el.selected) || el.getAttribute('aria-selected') === 'true', 'selected');
        const summary = el.tagName === 'SUMMARY' && el.parentElement && el.parentElement.tagName === 'DETAILS' && !el.hasAttribute('aria-expanded');
        const expanded = summary ? (el.parentElement.open ? 'true' : 'false') : el.getAttribute('aria-expanded');
        if (expanded) out.push('[expanded=' + expanded + ']');
        flag(role === 'dialog' && el.matches(':modal'), 'modal');
        if (role === 'heading') out.push('[level=' + (el.getAttribute('aria-level') || (/^H[1-6]$/.test(el.tagName) ? el.tagName[1] : '2')) + ']');
        if (role === 'link') out.push('[href=' + cut(el.getAttribute('href') || '', 120) + ']');
        return out;
    };
    const valueOf = (el, role, name) => {
        if ((el.tagName === 'PROGRESS' || el.tagName === 'METER') && (el.tagName === 'METER' || el.hasAttribute('value'))) return el.value + '/' + el.max;
        if (el.tagName === 'SELECT' && !el.multiple) { const option = el.selectedOptions[0]; return option ? clean(option.label) : ''; }
        const field = el.tagName === 'INPUT' || el.tagName === 'TEXTAREA';
        if (field && ['textbox', 'searchbox', 'spinbutton', 'slider', 'combobox'].includes(role)) return el.type !== 'password' ? el.value : '';
        if (['spinbutton', 'slider', 'progressbar', 'scrollbar', 'meter'].includes(role)) return clean(el.getAttribute('aria-valuetext') || el.getAttribute('aria-valuenow') || '');
        if (role === 'combobox') { const shown = textOf(el); return shown !== name ? shown : ''; }
        return '';
    };
    const childrenOf = node => {
        const shadow = node.shadowRoot || (typeof __bcm_closed_roots__ !== 'undefined' && __bcm_closed_roots__.get(node));
        if (shadow) return Array.from(shadow.childNodes);
        if (node.tagName === 'SLOT') { const assigned = node.assignedNodes({ flatten: true }); return assigned.length ? assigned : Array.from(node.childNodes); }
        return Array.from(node.childNodes);
    };
    const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'TITLE']);
    const walk = (node, depth) => {
        for (const child of childrenOf(node)) {
            if (truncated) return;
            if (child.nodeType === 3) { if (!outsideModal(child)) addText(child.data, depth); }
            else if (child.nodeType === 1 && !SKIP.has(child.tagName)) visit(child, depth);
        }
    };
    const shown = (el, contents) => contents || (el.checkVisibility ? el.checkVisibility({ visibilityProperty: true }) : el.getClientRects().length > 0);
    const visit = (el, depth) => {
        const view = el.ownerDocument.defaultView, style = view.getComputedStyle(el);
        const contents = style.display === 'contents';
        if (el.getAttribute('aria-hidden') === 'true' || !shown(el, contents)) return;
        if (el.inert || outsideModal(el)) return;
        let role = roleOf(el);
        const parent = el.parentElement;
        if (!role && style.cursor === 'pointer' && parent && view.getComputedStyle(parent).cursor !== 'pointer') role = 'clickable';
        if (!role) {
            const block = !contents && !style.display.startsWith('inline');
            if (block) flush();
            walk(el, depth);
            if (block) flush();
            return;
        }
        flush();
        const holder = role === 'clickable' && !!el.querySelector(INTERACTIVE);
        const name = nameOf(el, holder ? 'group' : role), value = valueOf(el, role, name), at = lines.length;
        const parts = [role];
        if (name) parts.push(JSON.stringify(cut(name, 100)));
        parts.push(...details(el, role), '[ref=' + refOf(el) + ']');
        push(depth, parts.join(' ') + (value ? ': ' + JSON.stringify(cut(value, 100)) : ''));
        if (role === 'iframe') {
            let doc = null; try { doc = el.contentDocument; } catch (e) {}
            if (!doc || !doc.body) {
                push(depth + 1, 'text: (another origin: not readable)');
                if (!truncated) foreign.push(refOf(el));
            }
            else if (depth < maxDepth) { walk(doc.body, depth + 1); flush(); }
            return;
        }
        if ((LEAF.has(role) && !holder) || depth >= maxDepth) return;
        walk(el, depth + 1);
        flush();
        if (NAMED_BY_TEXT.has(role) && !name) foldText(at, depth, role);
    };
    if (root.nodeType === 9) walk(root.body || root.documentElement, 0); else visit(root, 0);
    flush();
    return { lines, truncated, foreign };
};"#;

/// `snapshot`'s outline of the page, or of the element `selector` names
/// (CSS, `>>>`, or a ref), at most `depth` levels deep and `max_chars`
/// long: `{status: 'missing'}` or `{status: 'ok', url, title, text,
/// truncated, foreign}` (`foreign`: see the outline).
pub fn snapshot(selector: Option<&str>, depth: Option<u32>, max_chars: usize) -> String {
    let target = selector.map_or_else(|| "null".to_string(), string_literal);
    let depth = depth.unwrap_or(MAX_DEPTH);
    format!(
        "(() => {{ {RESOLVE} {OUTLINE} const target = {target}; \
         const root = target === null ? document : resolve(target); \
         if (!root) return {{ status: 'missing' }}; \
         const result = outline(root, {depth}, {max_chars}); \
         return {{ status: 'ok', url: location.href, title: document.title, text: result.lines.join('\\n'), truncated: result.truncated, foreign: result.foreign }}; }})()"
    )
}

/// `find_in_snapshot`'s search of the whole page's outline: the lines that
/// contain `text` (case-insensitive) or match `regex`, each under its
/// ancestors, at most [`FIND_MAX_MATCHES`] of them (a `- …` line marks
/// skipped lines): `{status: 'ok', url, matches, text, foreign}` (the
/// unreadable frames, see the outline). An invalid regex throws.
pub fn find(text: Option<&str>, regex: Option<&str>) -> String {
    format!(
        "(() => {{ {OUTLINE} const {{ lines, foreign }} = outline(document, {MAX_DEPTH}, {FIND_MAX_CHARS}); \
         return Object.assign({}, {{ foreign }}); }})()",
        filter_call("lines", text, regex)
    )
}

/// [`find`]'s search over `lines`, an outline the server put together (the
/// page's with its frames of another origin spliced in).
pub fn find_in_lines(lines: &[&str], text: Option<&str>, regex: Option<&str>) -> String {
    let lines = serde_json::to_string(lines).unwrap_or_else(|_| "[]".to_string());
    format!(
        "(() => {{ const lines = {lines}; return {}; }})()",
        filter_call("lines", text, regex)
    )
}

/// The search of `find` over the outline lines `lines` (a JavaScript
/// expression): `{status: 'ok', url, matches, text}`.
fn filter_call(lines: &str, text: Option<&str>, regex: Option<&str>) -> String {
    let literal = |value: Option<&str>| value.map_or_else(|| "null".to_string(), string_literal);
    format!(
        "((lines, text, pattern) => {{ \
         const re = pattern === null ? null : new RegExp(pattern); \
         const test = re ? content => re.test(content) : content => content.toLowerCase().includes(text.toLowerCase()); \
         const depthOf = line => line.search(/\\S/) / 2; \
         const shown = new Set(); let matches = 0; \
         lines.forEach((line, i) => {{ \
           if (!test(line.slice(line.indexOf('- ') + 2))) return; \
           matches++; if (matches > {FIND_MAX_MATCHES}) return; \
           shown.add(i); let depth = depthOf(line); \
           for (let j = i - 1; j >= 0 && depth > 0; j--) {{ const d = depthOf(lines[j]); if (d < depth) {{ shown.add(j); depth = d; }} }} \
         }}); \
         const out = []; let previous = -1; \
         for (const i of [...shown].sort((a, b) => a - b)) {{ \
           if (previous >= 0 && i !== previous + 1) out.push('  '.repeat(depthOf(lines[i])) + '- …'); \
           out.push(lines[i]); previous = i; \
         }} \
         return {{ status: 'ok', url: location.href, matches, text: out.join('\\n') }}; }})({lines}, {}, {})",
        literal(text),
        literal(regex),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first JSON string literal after `marker` in `expression`.
    fn literal_after(expression: &str, marker: &str) -> String {
        let start = expression.find(marker).unwrap() + marker.len();
        let mut values = serde_json::Deserializer::from_str(&expression[start..]).into_iter();
        values.next().unwrap().unwrap()
    }

    /// ISSUE-4: the outline follows the rendered tree (shadow roots, slots,
    /// same-origin iframes), leaves hidden elements out, keeps refs in the
    /// map the resolver reads, and never prints a password.
    #[test]
    fn outline_follows_the_rendered_tree() {
        let expression = snapshot(None, None, SNAPSHOT_MAX_CHARS);
        for needle in [
            "node.shadowRoot",
            "assignedNodes({ flatten: true })",
            "el.contentDocument",
            "checkVisibility",
            "aria-hidden",
            "new WeakRef(el)",
            "el.type !== 'password'",
            "Symbol.for('browser-control-mcp.refs')",
        ] {
            assert!(expression.contains(needle), "{needle}");
        }
        // The resolver reads the very map the outline fills
        let key = "Symbol.for('browser-control-mcp.refs')";
        assert!(RESOLVE.contains(key) && OUTLINE.contains(key));
        assert!(expression.contains(&format!(", {SNAPSHOT_MAX_CHARS});")));
        assert!(expression.contains("const target = null;"));
    }

    /// ISSUE-20: tables without headers are layout (their table, rows and
    /// cells leave only their content), and a cell is named by its content
    /// only when that content is plain text (folded into the name), so its
    /// children never repeat it.
    #[test]
    fn tables_do_not_repeat_their_text() {
        for needle in [
            "const layoutTable",
            "table.tHead",
            "table.caption",
            "cell.tagName === 'TH'",
        ] {
            assert!(OUTLINE.contains(needle), "{needle}");
        }
        let from_content = OUTLINE
            .split("const FROM_CONTENT = new Set([")
            .nth(1)
            .and_then(|rest| rest.split("]);").next())
            .unwrap();
        assert!(!from_content.contains("'cell'") && !from_content.contains("'columnheader'"));
        assert!(
            OUTLINE.contains("const foldText"),
            "cells of plain text fold it into their name"
        );
    }

    /// ISSUE-25: a clickable that holds interactive elements is not a
    /// leaf named from its whole text: its contents are listed.
    #[test]
    fn clickable_containers_list_their_contents() {
        assert!(OUTLINE.contains("const INTERACTIVE = "), "{OUTLINE}");
        assert!(
            OUTLINE.contains("const holder = role === 'clickable' && "),
            "{OUTLINE}"
        );
        assert!(OUTLINE.contains("(LEAF.has(role) && !holder)"), "{OUTLINE}");
    }

    /// ISSUE-29: ARIA widgets show their value: a combobox input its text,
    /// a spinbutton or slider its aria-valuetext (else aria-valuenow), and
    /// a combobox that is not a field the text it shows, unless that is
    /// already its name.
    #[test]
    fn aria_widgets_show_their_value() {
        assert!(
            OUTLINE.contains(
                "['textbox', 'searchbox', 'spinbutton', 'slider', 'combobox'].includes(role)"
            ),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE
                .contains("el.getAttribute('aria-valuetext') || el.getAttribute('aria-valuenow')"),
            "{OUTLINE}"
        );
        assert!(OUTLINE.contains("valueOf(el, role, name)"), "{OUTLINE}");
    }

    /// ISSUE-40: progress bars and meters show their value, a details'
    /// summary whether it is expanded, a modal dialog is marked, and what
    /// the page made inert (an inert subtree, everything outside an open
    /// modal dialog, text included) is left out.
    #[test]
    fn widgets_states_and_inert_content() {
        assert!(
            OUTLINE.contains("PROGRESS: 'progressbar', METER: 'meter'"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("if ((el.tagName === 'PROGRESS' || el.tagName === 'METER') && "),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("el.parentElement.open ? 'true' : 'false'"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("flag(role === 'dialog' && el.matches(':modal'), 'modal');"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("if (el.inert || outsideModal(el)) return;"),
            "{OUTLINE}"
        );
        assert!(OUTLINE.contains("if (child.nodeType === 3) { if (!outsideModal(child)) addText(child.data, depth); }"), "{OUTLINE}");
    }

    /// ISSUE-42: the outline walks the closed shadow roots it is handed
    /// (Chrome finds them; the page's script cannot).
    #[test]
    fn closed_shadow_roots_are_walked_when_handed() {
        assert!(
            OUTLINE.contains(
                "typeof __bcm_closed_roots__ !== 'undefined' && __bcm_closed_roots__.get(node)"
            ),
            "{OUTLINE}"
        );
    }

    /// ISSUE-27: the outline keeps the iframes it cannot read (their ref
    /// and element) for the server, and takes a size budget; the search
    /// runs the same filter over lines the server put together.
    #[test]
    fn unreadable_frames_are_kept_for_the_server() {
        assert!(
            OUTLINE.contains("if (!truncated) foreign.push(refOf(el));"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("return { lines, truncated, foreign };"),
            "{OUTLINE}"
        );
        let budget = snapshot(None, None, 1234);
        assert!(budget.contains("outline(root, 1000, 1234)"), "{budget}");
        assert!(budget.contains("foreign: result.foreign"), "{budget}");
        let lines = find_in_lines(&["- button \"a\" [ref=f2e1]"], Some("a"), None);
        assert!(
            lines.contains(r#"const lines = ["- button \"a\" [ref=f2e1]"]"#),
            "{lines}"
        );
        let page = find(Some("a"), None);
        assert!(
            page.contains("const { lines, foreign } = outline(document"),
            "{page}"
        );
    }

    /// ISSUE-4: the scope and the search reach JavaScript as one literal.
    #[test]
    fn scope_and_search_are_literals() {
        let crafted = r#"a"); window.__pwned = 1; ("#;
        let scoped = snapshot(Some(crafted), Some(2), SNAPSHOT_MAX_CHARS);
        assert_eq!(literal_after(&scoped, "const target = "), crafted);
        assert!(scoped.contains("outline(root, 2, "), "{scoped}");
        let found = find(Some(crafted), None);
        assert_eq!(literal_after(&found, "})(lines, "), crafted);
        assert!(
            found.contains(&format!("{}, null)", string_literal(crafted))),
            "{found}"
        );
        assert!(find(None, Some("^button")).contains("})(lines, null, \"^button\")"));
    }
}
