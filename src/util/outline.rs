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

/// Most options of a native `<select>` that `snapshot` lists (a country
/// list has ~250): then a line says how many more.
pub const SNAPSHOT_MAX_OPTIONS: usize = 25;

/// Most options of a `<select>` an outline searched by `find_in_snapshot`
/// lists: all of them, in practice.
pub const FIND_MAX_OPTIONS: usize = 10_000;

/// Deepest level walked when no `depth` is given.
const MAX_DEPTH: u32 = 1000;

/// What the outline and the page audit (`handler::audit`) share, declared
/// at the top of their snippets: `refOf(el)`, an element's ref, kept in the
/// top window's map (the one `js::RESOLVE` reads), the same for an element
/// across outlines and audits while it is in the page (entries of removed
/// elements are dropped); `nameOf(el, role)`, its accessible name
/// (approximate: aria-labelledby, aria-label, labels, alt, an input
/// button's value, the content for roles named by it, a tree item's own
/// text without its nested items', title, placeholder);
/// and the text helpers `clean`, `cut`, `textOf` (with the text slotted
/// into a shadow tree, which `innerText` misses) and `idText`.
pub(crate) const ELEMENT_HELPERS: &str = r#"const refsKey = Symbol.for('browser-control-mcp.refs'), refKey = Symbol.for('browser-control-mcp.ref');
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
const FROM_CONTENT = new Set(['button', 'link', 'heading', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
    'option', 'switch', 'clickable', 'checkbox', 'radio']);
const flatText = node => {
    if (node.nodeType === 3) return node.data;
    if (node.nodeType !== 1 || ['SCRIPT', 'STYLE', 'TEMPLATE'].includes(node.tagName)) return '';
    const assigned = node.tagName === 'SLOT' ? node.assignedNodes({ flatten: true }) : [];
    const children = assigned.length ? assigned : Array.from((node.shadowRoot || node).childNodes);
    return children.map(flatText).join(' ');
};
const textOf = el => clean(el.innerText !== undefined ? el.innerText : el.textContent)
    || (el.querySelector && el.querySelector('slot') ? clean(flatText(el)) : '');
const idText = (el, attribute) => {
    const root = el.getRootNode(), ids = el.getAttribute(attribute);
    if (!ids || !root.getElementById) return '';
    return clean(ids.split(/\s+/).map(id => root.getElementById(id)).filter(Boolean).map(textOf).join(' '));
};
const ownText = el => {
    const parts = [];
    const take = node => {
        if (node.nodeType === 3) { parts.push(node.data); return; }
        if (node.nodeType !== 1 || ['SCRIPT', 'STYLE', 'TEMPLATE'].includes(node.tagName)) return;
        const nested = (node.getAttribute('role') || '').trim().split(/\s+/)[0];
        if (['group', 'tree', 'treeitem'].includes(nested) || (!nested && (node.tagName === 'UL' || node.tagName === 'OL'))) return;
        node.childNodes.forEach(take);
    };
    el.childNodes.forEach(take);
    return clean(parts.join(' '));
};
const nameOf = (el, role) => {
    const named = idText(el, 'aria-labelledby'); if (named) return named;
    const label = clean(el.getAttribute('aria-label')); if (label) return label;
    if (el.labels && el.labels.length) { const named = clean(Array.from(el.labels).map(textOf).join(' ')); if (named) return named; }
    if (el.tagName === 'IMG' || el.tagName === 'AREA' || (el.tagName === 'INPUT' && el.type === 'image')) return clean(el.getAttribute('alt'));
    if (el.tagName === 'INPUT' && ['button', 'submit', 'reset'].includes(el.type)) return clean(el.value) || el.type;
    if (FROM_CONTENT.has(role)) { const named = textOf(el); if (named) return named; }
    if (role === 'treeitem') { const own = ownText(el); if (own) return own; }
    return clean(el.getAttribute('title') || el.getAttribute('placeholder'));
};"#;

/// An element's CSS selector and opening tag, declared after
/// [`ELEMENT_HELPERS`] (they use its `clean` and `cut`): `selectorOf(el)`,
/// the shortest id or `tag:nth-of-type` path (at most 5 steps, `host >>>
/// inner` across open shadow roots), and `snippetOf(el)`, its opening tag
/// (150 characters at most). For `audit_page` and `get_issues`.
pub(crate) const ELEMENT_LOCATORS: &str = r#"const esc = text => window.CSS && CSS.escape ? CSS.escape(text) : text.replace(/[^\w-]/g, '\\$&');
const selectorOf = el => {
    const root = el.getRootNode(), parts = [];
    for (let node = el; node && node.nodeType === 1 && parts.length < 5; node = node.parentElement) {
        if (node.id && root.querySelectorAll && root.querySelectorAll('#' + esc(node.id)).length === 1) { parts.unshift('#' + esc(node.id)); break; }
        const siblings = node.parentNode && node.parentNode.children ? Array.from(node.parentNode.children).filter(c => c.tagName === node.tagName) : [];
        parts.unshift(node.tagName.toLowerCase() + (siblings.length > 1 ? ':nth-of-type(' + (siblings.indexOf(node) + 1) + ')' : ''));
    }
    return (root.host ? selectorOf(root.host) + ' >>> ' : '') + parts.join(' > ');
};
const snippetOf = el => { const html = el.outerHTML || '', end = html.indexOf('>'); return cut(clean(end > 0 ? html.slice(0, end + 1) : html), 150); };"#;

/// Declares `outline(root, maxDepth, maxChars, maxOptions)`: `{lines,
/// truncated, foreign}` for `root` (the document, or an element and its
/// subtree). A native `<select>` combobox lists its options under it, at
/// most `maxOptions` (its options have no box while it is closed, so the
/// walk would skip them).
/// Elements with a role get a ref ([`ELEMENT_HELPERS`], declared before
/// it). `foreign`
/// lists the refs of the iframes of another origin it could not read, so
/// the server can find each frame and outline it there (`handler::frames`).
const OUTLINE: &str = r#"const outline = (root, maxDepth, maxChars, maxOptions) => {
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
    const draggable = el => el.getAttribute('draggable') === 'true';
    const gridOf = el => { const table = el.closest('table'), role = table ? firstRole(table) : ''; return role === 'grid' || role === 'treegrid'; };
    const LIVE = { alert: 'assertive', status: 'polite', log: 'polite' };
    const live = el => { const politeness = el.getAttribute('aria-live'); return politeness && politeness !== 'off' ? politeness : LIVE[firstRole(el)] || ''; };
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
        if (tag === 'TD' && gridOf(el)) return 'gridcell';
        if (TAGS[tag]) return TAGS[tag];
        if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
        if (live(el)) return 'generic';
        return el.hasAttribute('onclick') || el.hasAttribute('tabindex') && el.tabIndex >= 0
            || draggable(el) || el.hasAttribute('ondrop') || el.hasAttribute('ondragover') ? 'clickable' : null;
    };
    const LEAF = new Set(['button', 'link', 'heading', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio',
        'img', 'slider', 'spinbutton', 'option', 'switch', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'clickable',
        'progressbar', 'meter']);
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
    const dropText = (at, depth, text) => {
        const line = '  '.repeat(depth + 1) + '- text: ' + cut(text, 300), index = lines.indexOf(line, at + 1);
        if (index > at) { size -= line.length + 1; lines.splice(index, 1); }
    };
    const INTERACTIVE = 'a[href], button, input, select, textarea, summary, [role], [onclick], [tabindex], [contenteditable]';
    const matches = (el, selector) => { try { return el.matches(selector); } catch (e) { return false; } };
    const invalid = el => {
        const aria = el.getAttribute('aria-invalid');
        return aria === null || aria === 'false' ? matches(el, ':user-invalid') : true;
    };
    const details = (el, role) => {
        const out = [], flag = (on, name) => { if (on) out.push('[' + name + ']'); };
        const box = el.type === 'checkbox' || el.type === 'radio';
        flag(box ? el.checked : el.getAttribute('aria-checked') === 'true', 'checked');
        flag(el.disabled || el.getAttribute('aria-disabled') === 'true', 'disabled');
        flag(el.required, 'required');
        flag(draggable(el), 'draggable');
        let why = '';
        if (invalid(el)) {
            why = idText(el, 'aria-errormessage') || idText(el, 'aria-describedby') || clean(el.validationMessage || '');
            out.push('[invalid]'); if (why) out.push('[error=' + JSON.stringify(cut(why, 100)) + ']');
        }
        const description = idText(el, 'aria-describedby') || clean(el.getAttribute('aria-description'));
        if (description && description !== why) out.push('[description=' + JSON.stringify(cut(description, 100)) + ']');
        const pressed = el.getAttribute('aria-pressed');
        if (['true', 'false', 'mixed'].includes(pressed)) out.push('[pressed=' + pressed + ']');
        const current = clean(el.getAttribute('aria-current'));
        if (current && current !== 'false') out.push('[current=' + cut(current, 20) + ']');
        const politeness = live(el);
        if (politeness) out.push('[live=' + politeness + ']');
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
    const listOptions = (select, depth) => {
        let listed = 0, more = 0;
        const option = (o, group, d) => {
            if (o.hidden) return;
            if (listed >= maxOptions) { more++; return; }
            listed++;
            const parts = ['option'], label = clean(o.label);
            const flag = (on, name) => { if (on) parts.push('[' + name + ']'); };
            if (label) parts.push(JSON.stringify(cut(label, 100)));
            flag(o.selected, 'selected'); flag(o.disabled || group.disabled, 'disabled');
            push(d, parts.join(' '));
        };
        for (const child of select.children) {
            if (child.tagName === 'OPTION') option(child, select, depth);
            if (child.tagName !== 'OPTGROUP') continue;
            if (listed < maxOptions) push(depth, 'group ' + JSON.stringify(cut(clean(child.label), 100)));
            for (const o of child.children) if (o.tagName === 'OPTION') option(o, child, depth + 1);
        }
        if (more) push(depth, 'text: … ' + more + ' more options (find_in_snapshot searches them; fill_input picks one by its label or value)');
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
        if (role === 'combobox' && el.tagName === 'SELECT') {
            if (depth < maxDepth) listOptions(el, depth + 1);
            return;
        }
        if ((LEAF.has(role) && !holder) || depth >= maxDepth) return;
        walk(el, depth + 1);
        flush();
        if (NAMED_BY_TEXT.has(role) && !name) foldText(at, depth, role);
        if (role === 'treeitem' && name) dropText(at, depth, name);
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
    // A budget above the snapshot's is a whole page read for a search.
    let options = match max_chars > SNAPSHOT_MAX_CHARS {
        true => FIND_MAX_OPTIONS,
        false => SNAPSHOT_MAX_OPTIONS,
    };
    format!(
        "(() => {{ {RESOLVE} {ELEMENT_HELPERS} {OUTLINE} const target = {target}; \
         const root = target === null ? document : resolve(target); \
         if (!root) return {{ status: 'missing' }}; \
         const result = outline(root, {depth}, {max_chars}, {options}); \
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
        "(() => {{ {ELEMENT_HELPERS} {OUTLINE} const {{ lines, foreign }} = outline(document, {MAX_DEPTH}, {FIND_MAX_CHARS}, {FIND_MAX_OPTIONS}); \
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
        assert!(RESOLVE.contains(key) && ELEMENT_HELPERS.contains(key));
        assert!(expression.contains(&format!(", {SNAPSHOT_MAX_CHARS}, {SNAPSHOT_MAX_OPTIONS});")));
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
        let from_content = ELEMENT_HELPERS
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

    /// ISSUE-48: an element made draggable, or a drop zone (a drop or
    /// dragover handler attribute), has a role and a ref for `drag`; a
    /// draggable one says so (links and images, draggable by default, not).
    #[test]
    fn drag_sources_and_targets_have_refs() {
        assert!(
            OUTLINE.contains("const draggable = el => el.getAttribute('draggable') === 'true';"),
            "{OUTLINE}"
        );
        assert!(OUTLINE.contains("|| draggable(el) || el.hasAttribute('ondrop') || el.hasAttribute('ondragover') ? 'clickable' : null;"), "{OUTLINE}");
        assert!(
            OUTLINE.contains("flag(draggable(el), 'draggable');"),
            "{OUTLINE}"
        );
    }

    /// ISSUE-55: an element whose text is slotted into its shadow tree
    /// (`aria-labelledby` naming a `<span><slot></span>`, as web components
    /// do) is named by it: `innerText` does not see slotted nodes.
    #[test]
    fn slotted_text_names_elements() {
        assert!(
            ELEMENT_HELPERS.contains("node.tagName === 'SLOT'"),
            "{ELEMENT_HELPERS}"
        );
        assert!(
            ELEMENT_HELPERS.contains(
                "|| (el.querySelector && el.querySelector('slot') ? clean(flatText(el)) : '');"
            ),
            "{ELEMENT_HELPERS}"
        );
    }

    /// ISSUE-49: a field the page or the browser rejects is `[invalid]`
    /// (`aria-invalid`, or `:user-invalid` once the user changed it), with
    /// why: its error message, description or the browser's validation
    /// message.
    #[test]
    fn invalid_fields_say_why() {
        assert!(OUTLINE.contains("const invalid = el => {"), "{OUTLINE}");
        assert!(
            OUTLINE.contains(
                "aria === null || aria === 'false' ? matches(el, ':user-invalid') : true"
            ),
            "{OUTLINE}"
        );
        assert!(OUTLINE.contains("why = idText(el, 'aria-errormessage') || idText(el, 'aria-describedby') || clean(el.validationMessage || '');"), "{OUTLINE}");
        assert!(OUTLINE.contains("out.push('[invalid]'); if (why) out.push('[error=' + JSON.stringify(cut(why, 100)) + ']');"), "{OUTLINE}");
    }

    /// ISSUE-46: a native `<select>` lists its options under its combobox
    /// (labels, selected and disabled, optgroups as groups, no refs: they
    /// are picked with fill_input), at most a few in `snapshot` with a line
    /// saying how many more; the search sees them all.
    #[test]
    fn native_select_lists_its_options() {
        assert!(
            OUTLINE.contains("if (role === 'combobox' && el.tagName === 'SELECT') {"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains(
                "flag(o.selected, 'selected'); flag(o.disabled || group.disabled, 'disabled');"
            ),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains(
                "'text: … ' + more + ' more options (find_in_snapshot searches them; fill_input picks one by its label or value)'"
            ),
            "{OUTLINE}"
        );
        let snap = snapshot(None, None, SNAPSHOT_MAX_CHARS);
        assert!(
            snap.contains(&format!(
                "outline(root, 1000, {SNAPSHOT_MAX_CHARS}, {SNAPSHOT_MAX_OPTIONS})"
            )),
            "{snap}"
        );
        let search = find(Some("x"), None);
        assert!(
            search.contains(&format!(
                "outline(document, 1000, {FIND_MAX_CHARS}, {FIND_MAX_OPTIONS})"
            )),
            "{search}"
        );
        // The whole page read for a search across frames sees them all too
        let whole = snapshot(None, None, FIND_MAX_CHARS);
        assert!(whole.contains(&format!(", {FIND_MAX_OPTIONS})")), "{whole}");
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

    /// ISSUE-79: a tree item is named by its own text (not its nested
    /// items'), the cells of a grid are `gridcell`, and the outline shows
    /// descriptions, pressed and current states and live regions.
    #[test]
    fn names_descriptions_and_states_of_widgets() {
        assert!(
            ELEMENT_HELPERS.contains(
                "if (role === 'treeitem') { const own = ownText(el); if (own) return own; }"
            ),
            "{ELEMENT_HELPERS}"
        );
        assert!(
            ELEMENT_HELPERS.contains("['group', 'tree', 'treeitem'].includes(nested)"),
            "{ELEMENT_HELPERS}"
        );
        assert!(
            OUTLINE.contains("if (tag === 'TD' && gridOf(el)) return 'gridcell';"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains(
                "out.push('[description=' + JSON.stringify(cut(description, 100)) + ']');"
            ),
            "{OUTLINE}"
        );
        for state in ["pressed", "current", "live"] {
            assert!(
                OUTLINE.contains(&format!("out.push('[{state}=' + ")),
                "{state}: {OUTLINE}"
            );
        }
        assert!(
            OUTLINE
                .contains("const LIVE = { alert: 'assertive', status: 'polite', log: 'polite' };"),
            "{OUTLINE}"
        );
        assert!(
            OUTLINE.contains("if (live(el)) return 'generic';"),
            "{OUTLINE}"
        );
        // The tree item's own text is its name, not a line of its own too
        let dropped = "if (role === 'treeitem' && name) dropText(at, depth, name);";
        assert!(OUTLINE.contains(dropped), "{OUTLINE}");
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
        assert!(budget.contains("outline(root, 1000, 1234, "), "{budget}");
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
