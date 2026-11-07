//! `audit_page`: accessibility, SEO and best-practice checks of the
//! rendered page, the same JavaScript on every browser (one snippet in the
//! page, no Lighthouse bundle), plus the console errors the server logged
//! for the tab. Each failing check lists its first elements with the refs
//! `snapshot` gives, so the agent can act on them at once.

use crate::handler::McpHandler;
use crate::handler::cdp_domains::log::{LogState, canonical_level};
use crate::util::outline::{ELEMENT_HELPERS, ELEMENT_LOCATORS};
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

#[macros::mcp_tool(
    name = "audit_page",
    description = "Audits the rendered page for accessibility, SEO and best-practice problems, with the same checks on Chrome and Firefox (one script in the page, no Lighthouse): images without alt text; fields, buttons and links without a name; html lang; heading order; duplicate ids; broken aria references; positive tabindex; targets under 24x24 px; text contrast under WCAG AA; title, meta description, viewport, vague link text, uncrawlable links, canonical, noindex; doctype, HTTPS and mixed content, distorted images and console errors. Returns each failing check with its first 10 elements (ref, selector, snippet, detail), then the ids of the checks that passed or did not apply. 'categories' narrows it. Approximate, not a full WCAG review: text over background images, iframes and closed shadow roots are not checked."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct AuditPageTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Categories to check. Default: all three.
    pub categories: Option<Vec<AuditCategory>>,
}

/// A category of checks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum AuditCategory {
    /// Names, language, headings, ids, focus order, target size, contrast.
    #[serde(rename = "accessibility")]
    Accessibility,
    /// Title, description, viewport, links, canonical, indexing.
    #[serde(rename = "seo")]
    Seo,
    /// Doctype, HTTPS, mixed content, image aspect ratio, console errors.
    #[serde(rename = "best_practices")]
    BestPractices,
}

impl AuditCategory {
    /// Its name in the snippet and the reply.
    fn name(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::Seo => "seo",
            Self::BestPractices => "best_practices",
        }
    }
}

/// Every category, in the order of the reply.
const ALL: [AuditCategory; 3] = [
    AuditCategory::Accessibility,
    AuditCategory::Seo,
    AuditCategory::BestPractices,
];

/// Most elements a failing check lists (the count gives the rest).
const MAX_ELEMENTS: usize = 10;

/// Most console errors the `errors-in-console` check lists.
const MAX_ERRORS: usize = 10;

/// Declares `audit(wanted, maxElements)`: `{url, audits: [{category, id,
/// title, checked, count, elements: [{ref?, selector?, snippet?,
/// detail?}]}], notes}` for the categories in `wanted`. `checked` is how
/// many things a check looked at (0: it did not apply), `count` how many
/// failed. It walks the document and its open shadow roots (at most 20 000
/// elements) and needs [`ELEMENT_HELPERS`] and [`ELEMENT_LOCATORS`]
/// declared before it.
const AUDIT: &str = r#"const audit = (wanted, maxElements) => {
    const doc = document, notes = [], audits = [], elements = [], MAX = 20000;
    const collect = root => {
        const walker = doc.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
        for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            if (elements.length >= MAX) return;
            elements.push(node);
            if (node.shadowRoot) collect(node.shadowRoot);
        }
    };
    collect(doc);
    if (elements.length >= MAX) notes.push('the page has over ' + MAX + ' elements: only the first ' + MAX + ' were checked');
    if (elements.some(el => el.tagName === 'IFRAME' || el.tagName === 'FRAME')) notes.push('the content of iframes is not audited');
    const matches = (el, selector) => { try { return el.matches(selector); } catch (e) { return false; } };
    const roleAttr = el => (el.getAttribute('role') || '').trim().split(/\s+/)[0];
    const shown = el => el.checkVisibility ? el.checkVisibility({ visibilityProperty: true }) : el.getClientRects().length > 0;
    const exposed = el => shown(el) && !el.closest('[aria-hidden="true"], [inert]');
    const add = (category, id, title, failed, checked) => audits.push({ category, id, title, checked, count: failed.length,
        elements: failed.slice(0, maxElements).map(({ el, detail }) => Object.assign(
            el ? { ref: refOf(el), selector: selectorOf(el), snippet: snippetOf(el) } : {}, detail ? { detail } : {})) });
    const contentName = el => nameOf(el, 'button') || Array.from(el.querySelectorAll('img[alt], [aria-label], [title], svg title'))
        .map(n => n.tagName === 'IMG' ? n.getAttribute('alt') : n.getAttribute('aria-label') || n.getAttribute('title') || n.textContent)
        .map(clean).find(Boolean) || '';
    const isRole = (el, roles) => roles.includes(roleAttr(el));
    if (wanted.includes('accessibility')) {
        const svgTitle = el => el.tagName.toLowerCase() === 'svg' && el.querySelector('title') ? clean(el.querySelector('title').textContent) : '';
        const images = elements.filter(el => (el.tagName === 'IMG' || matches(el, 'input[type=image]') || roleAttr(el) === 'img')
            && !isRole(el, ['presentation', 'none']) && exposed(el));
        const imageNamed = el => (el.tagName === 'IMG' && el.hasAttribute('alt')) || !!(nameOf(el, 'img') || clean(el.getAttribute('title')) || svgTitle(el));
        add('accessibility', 'image-alt', 'Images without alternative text: add alt (alt="" marks a decorative image)',
            images.filter(el => !imageNamed(el)).map(el => ({ el })), images.length);
        const FIELD = 'input:not([type=hidden]):not([type=button]):not([type=submit]):not([type=reset]):not([type=image]), select, textarea, [role=textbox], [role=searchbox], [role=combobox]';
        const fields = elements.filter(el => matches(el, FIELD) && exposed(el));
        add('accessibility', 'label', 'Form fields without an accessible name: give them a <label>, aria-label or aria-labelledby',
            fields.filter(el => !nameOf(el, 'textbox')).map(el => ({ el })), fields.length);
        const buttons = elements.filter(el => (matches(el, 'button, input[type=button], input[type=submit], input[type=reset]') || roleAttr(el) === 'button') && exposed(el));
        add('accessibility', 'button-name', 'Buttons without an accessible name: give them text or aria-label',
            buttons.filter(el => !contentName(el)).map(el => ({ el })), buttons.length);
        const links = elements.filter(el => (matches(el, 'a[href]') || roleAttr(el) === 'link') && exposed(el));
        add('accessibility', 'link-name', 'Links without an accessible name: give them text or aria-label',
            links.filter(el => !contentName(el)).map(el => ({ el })), links.length);
        const html = doc.documentElement, lang = html.getAttribute('lang');
        let validLang = false; try { validLang = !!lang && !!lang.trim() && Intl.getCanonicalLocales(lang.trim()).length > 0; } catch (e) {}
        add('accessibility', 'html-lang', 'The page does not declare a valid language: add <html lang="...">',
            validLang ? [] : [{ el: html, detail: lang === null ? 'no lang attribute' : 'lang ' + JSON.stringify(lang) + ' is not a language tag' }], 1);
        const headings = elements.filter(el => (/^H[1-6]$/.test(el.tagName) && !roleAttr(el) || roleAttr(el) === 'heading') && exposed(el));
        const skipped = []; let previous = 0;
        for (const h of headings) {
            const level = Number(h.getAttribute('aria-level')) || (/^H[1-6]$/.test(h.tagName) ? Number(h.tagName[1]) : 2);
            if (previous && level > previous + 1) skipped.push({ el: h, detail: 'level ' + level + ' after level ' + previous });
            previous = level;
        }
        add('accessibility', 'heading-order', 'Headings skip levels: go down one level at a time', skipped, headings.length);
        const byRoot = new Map();
        for (const el of elements) if (el.id) {
            const root = el.getRootNode(); if (!byRoot.has(root)) byRoot.set(root, new Map());
            const ids = byRoot.get(root); ids.set(el.id, (ids.get(el.id) || []).concat(el));
        }
        const duplicated = [];
        for (const ids of byRoot.values()) for (const [id, all] of ids) if (all.length > 1) duplicated.push({ el: all[1], detail: 'id ' + JSON.stringify(id) + ' is used ' + all.length + ' times' });
        add('accessibility', 'duplicate-id', 'Ids used more than once: labels and aria references may point at the wrong element',
            duplicated, elements.filter(el => el.id).length);
        const REFERENCES = ['aria-labelledby', 'aria-describedby'];
        const referring = elements.filter(el => REFERENCES.some(a => el.hasAttribute(a)));
        const broken = [];
        for (const el of referring) for (const attribute of REFERENCES) {
            const root = el.getRootNode();
            const missing = (el.getAttribute(attribute) || '').split(/\s+/).filter(id => id && root.getElementById && !root.getElementById(id));
            if (missing.length) broken.push({ el, detail: attribute + ' names missing id ' + missing.map(id => JSON.stringify(id)).join(', ') });
        }
        add('accessibility', 'aria-references', 'aria-labelledby or aria-describedby name ids that do not exist', broken, referring.length);
        const ordered = elements.filter(el => el.hasAttribute('tabindex'));
        add('accessibility', 'tabindex', 'tabindex above 0 changes the focus order: use 0 or -1',
            ordered.filter(el => Number(el.getAttribute('tabindex')) > 0).map(el => ({ el, detail: 'tabindex ' + el.getAttribute('tabindex') })), ordered.length);
        const TARGET = 'a[href], button, input:not([type=hidden]), select, textarea, summary, [role=button], [role=link], [role=checkbox], [role=radio], [role=tab], [role=menuitem], [onclick]';
        const targets = elements.filter(el => matches(el, TARGET) && !el.disabled && exposed(el))
            .map(el => ({ el, rect: el.getBoundingClientRect() })).filter(t => t.rect.width > 0 && t.rect.height > 0).slice(0, 2000);
        const inSentence = el => {
            if (el.tagName !== 'A' || getComputedStyle(el).display !== 'inline') return false;
            let block = el.parentElement;
            while (block && getComputedStyle(block).display.startsWith('inline')) block = block.parentElement;
            return !!block && clean(block.textContent).length > clean(el.textContent).length;
        };
        const labelled = el => el.tagName === 'INPUT' && ['checkbox', 'radio'].includes(el.type) && labelsOf(el).length > 0;
        const small = t => t.rect.width < 24 || t.rect.height < 24;
        const center = r => [r.left + r.width / 2, r.top + r.height / 2];
        const distance = ([x, y], r) => Math.hypot(Math.max(r.left - x, 0, x - r.right), Math.max(r.top - y, 0, y - r.bottom));
        const crowded = t => {
            const c = center(t.rect);
            return targets.some(o => o !== t && !o.el.contains(t.el) && !t.el.contains(o.el)
                && (distance(c, o.rect) < 12 || (small(o) && Math.hypot(c[0] - center(o.rect)[0], c[1] - center(o.rect)[1]) < 24)));
        };
        add('accessibility', 'target-size', 'Targets under 24x24 px too close to another target: make them larger or space them out',
            targets.filter(t => small(t) && !inSentence(t.el) && !labelled(t.el) && crowded(t))
                .map(t => ({ el: t.el, detail: Math.round(t.rect.width) + 'x' + Math.round(t.rect.height) + ' px' })), targets.length);
        const parseColor = text => {
            const m = /^rgba?\(([^)]+)\)$/.exec(text || ''); if (!m) return null;
            const p = m[1].split(/[\s,/]+/).filter(Boolean).map(parseFloat);
            return p.length >= 3 && !p.some(isNaN) ? [p[0], p[1], p[2], p.length > 3 ? p[3] : 1] : null;
        };
        const over = (top, bottom) => {
            const a = top[3] + bottom[3] * (1 - top[3]); if (!a) return [0, 0, 0, 0];
            return [0, 1, 2].map(i => (top[i] * top[3] + bottom[i] * bottom[3] * (1 - top[3])) / a).concat(a);
        };
        const backgroundOf = el => {
            const layers = [];
            for (let node = el; node; node = node.parentElement || node.getRootNode().host || null) {
                const style = getComputedStyle(node);
                if (style.backgroundImage !== 'none') return null;
                const color = parseColor(style.backgroundColor); if (!color) return null;
                if (color[3] > 0) { layers.push(color); if (color[3] >= 1) break; }
            }
            return layers.reverse().reduce((below, layer) => over(layer, below), [255, 255, 255, 1]);
        };
        const luminance = c => { const [r, g, b] = c.slice(0, 3).map(v => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); }); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
        const hex = c => '#' + c.slice(0, 3).map(v => Math.round(v).toString(16).padStart(2, '0')).join('');
        const NO_TEXT = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'TITLE', 'OPTION', 'HEAD']);
        const texts = elements.filter(el => !NO_TEXT.has(el.tagName) && Array.from(el.childNodes).some(n => n.nodeType === 3 && n.data.trim())
            && !matches(el, ':disabled') && !el.closest('[aria-disabled="true"]') && exposed(el)).slice(0, 3000);
        let unknown = 0; const faint = [];
        for (const el of texts) {
            const style = getComputedStyle(el), color = parseColor(style.color), size = parseFloat(style.fontSize);
            if (!size || (color && color[3] === 0)) continue;
            const background = color && backgroundOf(el);
            if (!background) { unknown++; continue; }
            const fg = over(color, background), [l1, l2] = [luminance(fg), luminance(background)].sort((a, b) => b - a);
            const ratio = (l1 + 0.05) / (l2 + 0.05), bold = Number(style.fontWeight) >= 700;
            const needed = size >= 24 || (bold && size >= 18.66) ? 3 : 4.5;
            if (ratio < needed) faint.push({ el, detail: 'contrast ' + ratio.toFixed(2) + ':1, needs ' + needed + ':1 (' + hex(fg) + ' on ' + hex(background) + ', ' + Math.round(size) + ' px)' });
        }
        if (unknown) notes.push(unknown + ' text element(s) over a background image or gradient, or in a colour format not read, were not checked for contrast');
        add('accessibility', 'color-contrast', 'Text with too little contrast against its background (WCAG AA)', faint, texts.length - unknown);
    }
    if (wanted.includes('seo')) {
        const title = clean(doc.title);
        add('seo', 'document-title', 'The page has no <title>', title ? [] : [{ el: doc.querySelector('title'), detail: 'empty or missing title' }], 1);
        const description = doc.querySelector('meta[name="description" i]');
        add('seo', 'meta-description', 'The page has no meta description',
            description && clean(description.getAttribute('content')) ? [] : [{ el: description, detail: description ? 'empty content' : 'no <meta name="description">' }], 1);
        const viewport = doc.querySelector('meta[name="viewport" i]'), content = viewport ? viewport.getAttribute('content') || '' : '';
        add('seo', 'viewport', 'The page has no viewport meta tag with a width or initial-scale',
            /width|initial-scale/i.test(content) ? [] : [{ el: viewport, detail: viewport ? 'content ' + JSON.stringify(content) : 'no <meta name="viewport">' }], 1);
        const VAGUE = new Set(['click here', 'click this', 'go', 'here', 'information', 'learn more', 'more', 'more info', 'more information',
            'right here', 'read more', 'see more', 'start', 'this', 'link']);
        const anchors = elements.filter(el => matches(el, 'a[href]') && exposed(el));
        add('seo', 'link-text', 'Links with text that does not say where they go',
            anchors.filter(el => !el.hasAttribute('aria-label') && VAGUE.has(textOf(el).toLowerCase().replace(/[.!?:…»›>]+$/, '').trim()))
                .map(el => ({ el, detail: JSON.stringify(textOf(el)) })), anchors.length);
        const allAnchors = elements.filter(el => el.tagName === 'A');
        add('seo', 'crawlable-anchors', 'Links a crawler cannot follow: give them a real href',
            allAnchors.filter(el => /^\s*javascript:/i.test(el.getAttribute('href') || '') || (!el.hasAttribute('href') && (el.hasAttribute('onclick') || roleAttr(el) === 'link')))
                .map(el => ({ el, detail: el.hasAttribute('href') ? 'javascript: href' : 'no href' })), allAnchors.length);
        const canonicals = Array.from(doc.querySelectorAll('link[rel~="canonical" i]')), wrong = [];
        if (canonicals.length > 1) wrong.push({ el: canonicals[1], detail: canonicals.length + ' canonical links' });
        for (const link of canonicals.slice(0, 1)) {
            let url = null; try { url = new URL(link.getAttribute('href') || '', location.href); } catch (e) {}
            if (!url || !link.getAttribute('href')) wrong.push({ el: link, detail: 'not a valid URL' });
            else if (location.protocol === 'https:' && url.protocol === 'http:') wrong.push({ el: link, detail: 'points to http: from an https: page' });
        }
        add('seo', 'canonical', 'The canonical link is not valid', wrong, canonicals.length);
        const robots = Array.from(doc.querySelectorAll('meta[name="robots" i], meta[name="googlebot" i]'));
        add('seo', 'is-crawlable', 'A robots meta tag keeps the page out of search results',
            robots.filter(m => /noindex|none/i.test(m.getAttribute('content') || '')).map(el => ({ el, detail: 'content ' + JSON.stringify(el.getAttribute('content')) })), 1);
    }
    if (wanted.includes('best_practices')) {
        const doctype = doc.doctype, quirks = doc.compatMode !== 'CSS1Compat';
        const doctypeIssue = !doctype ? 'no <!DOCTYPE html>' : doctype.name.toLowerCase() !== 'html' ? 'doctype ' + doctype.name : quirks ? 'the page renders in quirks mode' : null;
        add('best_practices', 'doctype', 'The page lacks <!DOCTYPE html> and renders in quirks mode', doctypeIssue ? [{ detail: doctypeIssue }] : [], 1);
        const local = ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
        add('best_practices', 'is-on-https', 'The page is served over http:',
            location.protocol === 'http:' && !local ? [{ detail: location.origin }] : [], 1);
        const mixed = [];
        if (location.protocol === 'https:') {
            const SOURCES = 'img[src], script[src], iframe[src], audio[src], video[src], source[src], embed[src], object[data], link[rel~="stylesheet" i][href], link[rel~="icon" i][href]';
            const seen = new Set();
            for (const el of elements.filter(el => matches(el, SOURCES))) {
                const url = el.src || el.data || el.href || '';
                if (/^http:/i.test(url)) { seen.add(url); mixed.push({ el, detail: cut(url, 150) }); }
            }
            for (const entry of performance.getEntriesByType('resource'))
                if (/^http:/i.test(entry.name) && !seen.has(entry.name)) { seen.add(entry.name); mixed.push({ detail: cut(entry.name, 150) }); }
        }
        add('best_practices', 'mixed-content', 'An https: page loads http: resources', mixed, location.protocol === 'https:' ? 1 : 0);
        const pictures = elements.filter(el => el.tagName === 'IMG' && el.complete && el.naturalWidth > 0 && el.naturalHeight > 0 && shown(el)
            && !/\.svg(\?|#|$)|^data:image\/svg/i.test(el.currentSrc || el.src) && ['fill', ''].includes(getComputedStyle(el).objectFit))
            .map(el => ({ el, rect: el.getBoundingClientRect() })).filter(p => p.rect.width > 5 && p.rect.height > 5);
        add('best_practices', 'image-aspect-ratio', 'Images shown with a distorted aspect ratio',
            pictures.filter(({ el, rect }) => { const natural = el.naturalWidth / el.naturalHeight; return Math.abs(rect.width / rect.height - natural) / natural > 0.1; })
                .map(({ el, rect }) => ({ el, detail: 'shown ' + (rect.width / rect.height).toFixed(2) + ', natural ' + (el.naturalWidth / el.naturalHeight).toFixed(2) })), pictures.length);
    }
    return { url: location.href, audits, notes };
};"#;

impl AuditPageTool {
    /// The categories asked for, in reply order (all by default).
    fn wanted(&self) -> Vec<AuditCategory> {
        match &self.categories {
            Some(asked) if !asked.is_empty() => {
                ALL.into_iter().filter(|c| asked.contains(c)).collect()
            }
            _ => ALL.to_vec(),
        }
    }

    /// The page's checks for the categories asked for.
    pub(crate) fn snippet(&self) -> String {
        let wanted: Vec<&str> = self.wanted().into_iter().map(AuditCategory::name).collect();
        let wanted = serde_json::to_string(&wanted).unwrap_or_else(|_| "[]".into());
        format!(
            "(() => {{ {ELEMENT_HELPERS} {ELEMENT_LOCATORS} {AUDIT} return audit({wanted}, {MAX_ELEMENTS}); }})()"
        )
    }

    /// The reply from what the page found and the tab's console (shared
    /// by every browser): a line with the totals, then the failing checks
    /// with their elements, the ids of the passed and not applicable ones,
    /// and notes.
    pub(crate) async fn report(&self, found: Value, log: &Arc<Mutex<LogState>>) -> CallToolResult {
        let mut audits = found["audits"].as_array().cloned().unwrap_or_default();
        if self.wanted().contains(&AuditCategory::BestPractices) {
            audits.push(console_audit(log).await);
        }
        let url = found["url"].as_str().unwrap_or_default();
        let notes = found["notes"].clone();
        CallToolResult::text_content(vec![summarize(&self.wanted(), url, audits, notes).into()])
    }

    /// Chrome: runs the checks in the tab's page and reads its console.
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: AuditPageTool = crate::handler::parse_args(params)?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target = session.target(args.tab_id.clone()).await?;
        let found = crate::handler::snippet_value(&target, &args.snippet()).await?;
        let log = session.log_state(args.tab_id.clone())?;
        Ok(args.report(found, &log).await)
    }
}

/// The `errors-in-console` check: the errors the tab's console holds.
async fn console_audit(log: &Arc<Mutex<LogState>>) -> Value {
    let state = log.lock().await;
    let errors: Vec<Value> = state
        .messages
        .iter()
        .filter(|m| canonical_level(&m.level) == "error")
        .map(|m| {
            let at = match (&m.url, m.line_number) {
                (Some(url), Some(line)) => format!(" @ {url}:{line}"),
                (Some(url), None) => format!(" @ {url}"),
                _ => String::new(),
            };
            json!({"detail": crate::util::text::capped(&format!("{}{at}", m.text), 200)})
        })
        .collect();
    json!({
        "category": "best_practices",
        "id": "errors-in-console",
        "title": "The page logged errors to the console (get_console_logs shows them whole)",
        "checked": 1,
        "count": errors.len(),
        "elements": errors.into_iter().take(MAX_ERRORS).collect::<Vec<_>>(),
    })
}

/// Whether a check found something.
fn failing(audit: &Value) -> bool {
    audit["count"].as_u64().unwrap_or(0) > 0
}

/// Whether a check had something to look at.
fn applies(audit: &Value) -> bool {
    audit["checked"].as_u64().unwrap_or(0) > 0
}

/// The ids of the checks `keep` picks.
fn ids(audits: &[Value], keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    audits
        .iter()
        .filter(|a| keep(a))
        .map(|a| a["id"].clone())
        .collect()
}

/// `{failed, passed, not_applicable}` per category, and `name failed`
/// for the totals line.
fn per_category(
    wanted: &[AuditCategory],
    audits: &[Value],
) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut summary = serde_json::Map::new();
    let mut totals = Vec::new();
    for category in wanted {
        let own: Vec<&Value> = audits
            .iter()
            .filter(|a| a["category"] == category.name())
            .collect();
        let failed = own.iter().filter(|a| failing(a)).count();
        let passed = own.iter().filter(|a| !failing(a) && applies(a)).count();
        let not_applicable = own.len() - failed - passed;
        totals.push(format!("{} {failed}", category.name()));
        summary.insert(
            category.name().into(),
            json!({"failed": failed, "passed": passed, "not_applicable": not_applicable}),
        );
    }
    (summary, totals)
}

/// The reply's text: the totals line and the JSON body.
fn summarize(wanted: &[AuditCategory], url: &str, audits: Vec<Value>, notes: Value) -> String {
    let (summary, totals) = per_category(wanted, &audits);
    let passed = ids(&audits, |a| !failing(a) && applies(a));
    let not_applicable = ids(&audits, |a| !failing(a) && !applies(a));
    let failed: Vec<Value> = audits
        .into_iter()
        .filter(failing)
        .map(|mut a| {
            if let Some(fields) = a.as_object_mut() {
                fields.remove("checked");
            }
            a
        })
        .collect();
    let first = format!(
        "Audit of {url}: {} failing check(s) ({}). Elements carry snapshot refs for click_element, scroll or capture_screenshot.",
        failed.len(),
        totals.join(", ")
    );
    let mut body = json!({
        "url": url,
        "summary": summary,
        "failed": failed,
        "passed": passed,
        "not_applicable": not_applicable,
    });
    if notes.as_array().is_some_and(|n| !n.is_empty()) {
        body["notes"] = notes;
    }
    format!(
        "{first}\n{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::log::ConsoleMessage;

    /// ISSUE-104: a checkbox or radio's labels come from the shared map
    /// (`labelsOf`), not `el.labels`, which scans the document per element.
    #[test]
    fn labels_come_from_the_shared_map() {
        assert!(AUDIT.contains("labelsOf(el).length"), "{AUDIT}");
        assert!(!AUDIT.contains("el.labels"), "{AUDIT}");
    }

    fn tool(args: Value) -> AuditPageTool {
        serde_json::from_value(args).unwrap()
    }

    fn message(level: &str, text: &str) -> ConsoleMessage {
        ConsoleMessage {
            source: "javascript".into(),
            level: level.into(),
            text: text.into(),
            timestamp: 0.0,
            url: Some("http://a/app.js".into()),
            line_number: Some(3),
            channel: Default::default(),
        }
    }

    fn log(messages: Vec<ConsoleMessage>) -> Arc<Mutex<LogState>> {
        let mut state = LogState::default();
        state.messages = messages.into();
        Arc::new(Mutex::new(state))
    }

    /// ISSUE-55: the snippet runs the shared element helpers (so its refs
    /// are snapshot's) and the categories asked for, all by default.
    #[test]
    fn snippet_runs_the_categories_asked_for() {
        let all = tool(json!({})).snippet();
        assert!(
            all.starts_with(&format!(
                "(() => {{ {ELEMENT_HELPERS} {ELEMENT_LOCATORS} {AUDIT}"
            )),
            "{all}"
        );
        assert!(
            all.ends_with(r#"return audit(["accessibility","seo","best_practices"], 10); })()"#),
            "{all}"
        );
        let seo = tool(json!({"categories": ["seo", "accessibility"]})).snippet();
        assert!(
            seo.ends_with(r#"return audit(["accessibility","seo"], 10); })()"#),
            "{seo}"
        );
    }

    /// ISSUE-55: the checks follow WCAG AA and Lighthouse's thresholds.
    #[test]
    fn checks_follow_the_published_thresholds() {
        for needle in [
            "const needed = size >= 24 || (bold && size >= 18.66) ? 3 : 4.5;",
            "const small = t => t.rect.width < 24 || t.rect.height < 24;",
            "(distance(c, o.rect) < 12 ||",
            "'click here', 'click this', 'go', 'here'",
            "Math.abs(rect.width / rect.height - natural) / natural > 0.1",
            "doc.compatMode !== 'CSS1Compat'",
            "Intl.getCanonicalLocales(lang.trim())",
        ] {
            assert!(AUDIT.contains(needle), "{needle}");
        }
    }

    /// ISSUE-55: links in a line of text are exempt from the target size
    /// (WCAG's inline exception: up to the first block ancestor, as a
    /// citation `<sup><a>[1]</a></sup>`), and a descendant's title names
    /// a link or button (an icon `<div title="upvote">`).
    #[test]
    fn inline_links_and_titled_icons_pass() {
        assert!(
            AUDIT.contains("while (block && getComputedStyle(block).display.startsWith('inline')) block = block.parentElement;"),
            "{AUDIT}"
        );
        assert!(
            AUDIT.contains("querySelectorAll('img[alt], [aria-label], [title], svg title')"),
            "{AUDIT}"
        );
    }

    /// ISSUE-55: failing checks list their elements (without `checked`),
    /// the others are passed or not applicable, per category; console
    /// errors fail best practices, warnings do not.
    #[tokio::test]
    async fn report_sorts_the_checks() {
        // Given a page with an image without alt, no headings and a title
        let found = json!({"url": "http://a/", "notes": ["the content of iframes is not audited"], "audits": [
            {"category": "accessibility", "id": "image-alt", "title": "Images without alternative text", "checked": 2, "count": 1,
             "elements": [{"ref": "e3", "selector": "img:nth-of-type(2)", "snippet": "<img src=\"a.png\">"}]},
            {"category": "accessibility", "id": "heading-order", "title": "Headings skip levels", "checked": 0, "count": 0, "elements": []},
            {"category": "seo", "id": "document-title", "title": "The page has no <title>", "checked": 1, "count": 0, "elements": []}]});
        // And a console with an error and a warning
        let log = log(vec![
            message("error", "boom"),
            message("warning", "careful"),
        ]);
        // When the report is made
        let reply = tool(json!({})).report(found, &log).await;
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        // Then the first line counts the failing checks per category
        let (first, body) = text.split_once('\n').unwrap();
        assert_eq!(
            first,
            "Audit of http://a/: 2 failing check(s) (accessibility 1, seo 0, best_practices 1). Elements carry snapshot refs for click_element, scroll or capture_screenshot."
        );
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body["summary"]["accessibility"],
            json!({"failed": 1, "passed": 0, "not_applicable": 1})
        );
        assert_eq!(body["failed"][0]["elements"][0]["ref"], "e3");
        assert!(body["failed"][0].get("checked").is_none(), "{body}");
        assert_eq!(body["failed"][1]["id"], "errors-in-console");
        assert_eq!(
            body["failed"][1]["elements"],
            json!([{"detail": "boom @ http://a/app.js:3"}])
        );
        assert_eq!(body["passed"], json!(["document-title"]));
        assert_eq!(body["not_applicable"], json!(["heading-order"]));
        assert_eq!(body["notes"][0], "the content of iframes is not audited");
    }

    /// ISSUE-55: without best practices the console is not a check.
    #[tokio::test]
    async fn console_counts_only_for_best_practices() {
        let found = json!({"url": "http://a/", "notes": [], "audits": []});
        let log = log(vec![message("error", "boom")]);
        let reply = tool(json!({"categories": ["seo"]}))
            .report(found, &log)
            .await;
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.starts_with("Audit of http://a/: 0 failing check(s) (seo 0)."),
            "{text}"
        );
        assert!(
            !text.contains("errors-in-console") && !text.contains("\"notes\""),
            "{text}"
        );
    }

    /// ISSUE-55 (Chrome): the checks run in the tab's page.
    #[tokio::test]
    async fn chrome_runs_the_checks_in_the_page() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        // Given a page whose checks find nothing
        let mock = spawn_recording_mock().await;
        mock.respond_with("Runtime.evaluate", |_| {
            json!({"result": {"result": {"type": "object", "value": {"url": "http://a/", "notes": [], "audits": []}}}})
        });
        let handler = mock_handler(mock.http_port);
        // When the page is audited
        let reply = AuditPageTool::handle(call("audit_page", json!({})), &handler)
            .await
            .unwrap();
        // Then the snippet ran there and only the console check remains
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(
            text.contains("\"passed\": [\n    \"errors-in-console\"\n  ]"),
            "{text}"
        );
        let sent = &mock.commands_named("Runtime.evaluate")[0].params;
        assert!(
            sent["expression"]
                .as_str()
                .unwrap()
                .contains("const audit = (wanted, maxElements)"),
            "{sent}"
        );
    }
}
