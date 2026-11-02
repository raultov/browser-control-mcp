//! `profile_page_performance`'s `insights`: what explains the numbers,
//! read in the page with the Web Performance APIs, the same JavaScript on
//! every browser (Chrome's trace has no ready-made explanation). Sections a
//! browser does not report are `null`, with a note; a few findings are
//! derived from the web.dev thresholds.

use serde_json::{Value, json};

/// The snippet: `{document, lcp, inp, cls, render_blocking, long_scripts,
/// slowest_resources, dom_size, images, third_parties, lcp_supported,
/// inp_supported}`, at most 5 entries per list.
pub(crate) const SNIPPET: &str = r#"(() => {
    const types = (window.PerformanceObserver && PerformanceObserver.supportedEntryTypes) || [];
    const take = type => {
        if (!types.includes(type)) return null;
        const observer = new PerformanceObserver(() => {});
        observer.observe({ type, buffered: true });
        const entries = observer.takeRecords(); observer.disconnect();
        return entries;
    };
    const ms = n => n == null || !isFinite(n) ? null : Math.round(n * 10) / 10;
    const url = text => !text ? null : text.length > 150 ? text.slice(0, 149) + '…' : text;
    const describe = el => {
        if (!el || !el.tagName) return null;
        let name = el.tagName.toLowerCase();
        if (el.id) name += '#' + el.id;
        else if (el.classList && el.classList.length) name += '.' + Array.from(el.classList).slice(0, 2).join('.');
        return name;
    };
    const top = (items, key) => items.sort((a, b) => b[key] - a[key]).slice(0, 5);
    const nav = performance.getEntriesByType('navigation')[0];
    const ttfb = nav ? nav.responseStart : null;
    const document_ = nav ? {
        ttfb_ms: ms(nav.responseStart), server_ms: ms(nav.responseStart - nav.requestStart),
        redirect_ms: ms(nav.redirectEnd - nav.redirectStart), transfer_size: nav.transferSize,
        encoded_body_size: nav.encodedBodySize, decoded_body_size: nav.decodedBodySize,
    } : null;
    const resources = performance.getEntriesByType('resource');
    const lcps = take('largest-contentful-paint');
    let lcp = null;
    if (lcps && lcps.length) {
        const entry = lcps[lcps.length - 1], start = ttfb || 0, lcpEl = entry.element;
        const res = entry.url ? resources.find(r => r.name === entry.url) : null;
        const loadStart = res ? (res.requestStart || res.startTime) : null, loadEnd = res ? res.responseEnd : null;
        const discovery = lcpEl && lcpEl.tagName === 'IMG' ? {
            lazy_loaded: lcpEl.loading === 'lazy' || lcpEl.getAttribute('loading') === 'lazy',
            fetch_priority: lcpEl.fetchPriority || lcpEl.getAttribute('fetchpriority') || 'auto',
        } : null;
        lcp = { time_ms: ms(entry.startTime), element: describe(lcpEl), url: url(entry.url), size: entry.size,
            breakdown: { ttfb_ms: ms(start), load_delay_ms: res ? ms(Math.max(0, loadStart - start)) : null,
                load_duration_ms: res ? ms(loadEnd - loadStart) : null,
                render_delay_ms: ms(entry.startTime - (res ? Math.max(loadEnd, start) : start)) },
            ...(discovery ? { discovery } : {}) };
    }
    const events = take('event');
    const interactions = events && events.filter(e => e.interactionId > 0 || ['click', 'keydown', 'pointerdown', 'pointerup'].includes(e.name));
    const worstIn = interactions && interactions.length ? top(interactions.slice(), 'duration')[0] : null;
    const worstEvent = worstIn && worstIn.interactionId
        ? interactions.filter(e => e.interactionId === worstIn.interactionId)
            .sort((a, b) => (b.processingEnd - b.processingStart) - (a.processingEnd - a.processingStart))[0]
        : worstIn;
    const inp = worstEvent ? {
        duration_ms: ms(worstEvent.duration), name: worstEvent.name, target: describe(worstEvent.target),
        input_delay_ms: ms(worstEvent.processingStart - worstEvent.startTime),
        processing_ms: ms(worstEvent.processingEnd - worstEvent.processingStart),
        presentation_delay_ms: ms(Math.max(0, worstEvent.startTime + worstEvent.duration - worstEvent.processingEnd)),
    } : null;
    const shifts = take('layout-shift');
    const cls = shifts && {
        score: Math.round(shifts.filter(s => !s.hadRecentInput).reduce((sum, s) => sum + s.value, 0) * 1000) / 1000,
        culprits: top(shifts.filter(s => !s.hadRecentInput).map(s => ({ value: Math.round(s.value * 1000) / 1000,
            at_ms: ms(s.startTime), moved: (s.sources || []).map(x => describe(x.node)).filter(Boolean).slice(0, 3) }))
            .filter(s => s.value > 0), 'value'),
    };
    const blocking = resources.some(r => 'renderBlockingStatus' in r)
        ? top(resources.filter(r => r.renderBlockingStatus === 'blocking')
            .map(r => ({ url: url(r.name), type: r.initiatorType, duration_ms: ms(r.duration), end_ms: ms(r.responseEnd) })), 'duration_ms')
        : null;
    const frames = take('long-animation-frame');
    const scripts = frames && top(frames.flatMap(f => Array.from(f.scripts || [])).map(s => ({
        url: url(s.sourceURL), function: s.sourceFunctionName || null, invoker: s.invoker || null,
        duration_ms: ms(s.duration), at_ms: ms(s.startTime) })), 'duration_ms');
    const slowest = top(resources.map(r => ({ url: url(r.name), type: r.initiatorType,
        duration_ms: ms(r.duration), transfer_size: r.transferSize })), 'duration_ms');
    const domSize = (() => {
        let nodes = 0, maxDepth = 0, maxChildren = 0, widest = null;
        const stack = document.documentElement ? [[document.documentElement, 1]] : [];
        while (stack.length) {
            const [node, depth] = stack.pop();
            nodes++;
            if (depth > maxDepth) maxDepth = depth;
            const children = node.children;
            if (children && children.length) {
                if (children.length > maxChildren) { maxChildren = children.length; widest = describe(node); }
                for (let i = children.length - 1; i >= 0; i--) stack.push([children[i], depth + 1]);
            }
        }
        return { nodes, max_depth: maxDepth, widest_element: widest, max_children: maxChildren };
    })();
    const dpr = window.devicePixelRatio || 1;
    const images = top(Array.from(document.querySelectorAll('img')).map(img => {
        const r = img.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0 || !img.complete || !img.naturalWidth) return null;
        const natural = img.naturalWidth * img.naturalHeight, shown = r.width * r.height * dpr * dpr;
        const oversized = natural > shown * 2 && natural - shown > 100000;
        const missing = !img.hasAttribute('width') || !img.hasAttribute('height');
        if (!oversized && !missing) return null;
        return { element: describe(img), url: url(img.currentSrc || img.src),
            rendered_px: [Math.round(r.width), Math.round(r.height)],
            natural_px: [img.naturalWidth, img.naturalHeight],
            oversized, missing_dimensions: missing, _waste: Math.max(0, natural - shown) };
    }).filter(Boolean), '_waste').map(({ _waste, ...rest }) => rest);
    const byOrigin = new Map();
    for (const r of resources) {
        let origin = null;
        try { const u = new URL(r.name, location.href); if (u.origin !== location.origin && /^https?:$/.test(u.protocol)) origin = u.origin; } catch (e) {}
        if (!origin) continue;
        const entry = byOrigin.get(origin) || { origin, requests: 0, transfer_size: 0, total_duration_ms: 0 };
        entry.requests++;
        entry.transfer_size += r.transferSize || 0;
        entry.total_duration_ms = ms(entry.total_duration_ms + (r.duration || 0));
        byOrigin.set(origin, entry);
    }
    const thirdParties = top(Array.from(byOrigin.values()), 'total_duration_ms');
    return { document: document_, lcp, inp, cls, render_blocking: blocking, long_scripts: scripts, slowest_resources: slowest,
        dom_size: domSize, images, third_parties: thirdParties,
        lcp_supported: types.includes('largest-contentful-paint'), inp_supported: types.includes('event') };
})()"#;

/// The page's insights with notes for what the browser does not report,
/// and the findings they show.
pub(crate) fn annotate(mut found: Value) -> Value {
    let lcp_supported = found.get("lcp_supported") == Some(&Value::Bool(true));
    let inp_unsupported = found.get("inp_supported") == Some(&Value::Bool(false));
    let notes = missing_notes(&found, lcp_supported, inp_unsupported);
    let findings: Vec<String> = [
        lcp_finding(&found["lcp"]),
        lcp_discovery_finding(&found["lcp"]),
        inp_finding(&found["inp"]),
        cls_finding(&found["cls"]),
        server_finding(&found["document"]),
        compression_finding(&found["document"]),
        blocking_finding(&found["render_blocking"]),
        dom_finding(&found["dom_size"]),
        image_finding(&found["images"]),
    ]
    .into_iter()
    .flatten()
    .collect();
    let Some(object) = found.as_object_mut() else {
        return found;
    };
    object.remove("lcp_supported");
    object.remove("inp_supported");
    if !findings.is_empty() {
        object.insert("findings".into(), json!(findings));
    }
    if !notes.is_empty() {
        object.insert("notes".into(), json!(notes));
    }
    found
}

/// Most unused bytes of JavaScript or CSS before it is a finding.
const UNUSED_CODE_BYTES: u64 = 50_000;

/// Most forced-reflow time (ms) before it is a finding.
const FORCED_REFLOW_MS: f64 = 30.0;

/// Adds the section `key` to `insights` with its findings, or `null` and a
/// note saying why it is missing.
fn add_section(insights: &mut Value, key: &str, section: Result<(Value, Vec<String>), String>) {
    let Some(object) = insights.as_object_mut() else {
        return;
    };
    let push = |object: &mut serde_json::Map<String, Value>, list: &str, text: String| {
        let entries = object.entry(list).or_insert_with(|| json!([]));
        if let Some(entries) = entries.as_array_mut() {
            entries.push(json!(text));
        }
    };
    match section {
        Ok((value, findings)) => {
            for finding in findings {
                push(object, "findings", finding);
            }
            object.insert(key.into(), value);
        }
        Err(why) => {
            object.insert(key.into(), Value::Null);
            push(object, "notes", format!("{key} is null: {why}"));
        }
    }
}

/// Adds `unused_code` to `insights` (Chrome's coverage of a reload, see
/// `tracing::coverage`) with a finding when much of it went unused, or
/// `null` and a note saying why it is missing.
pub(crate) fn add_unused_code(insights: &mut Value, unused: Result<Value, String>) {
    let section = unused.map(|unused| {
        let findings = unused_code_findings(&unused);
        (unused, findings)
    });
    add_section(insights, "unused_code", section);
}

/// Adds `forced_reflows` to `insights` (from Chrome's trace) with a finding
/// when they took long, or `null` and a note saying why it is missing.
pub(crate) fn add_forced_reflows(insights: &mut Value, reflows: Result<Value, String>) {
    let section = reflows.map(|reflows| {
        let findings = forced_reflow_finding(&reflows).into_iter().collect();
        (reflows, findings)
    });
    add_section(insights, "forced_reflows", section);
}

/// Forced reflows over [`FORCED_REFLOW_MS`], with the call site that
/// forced most.
fn forced_reflow_finding(reflows: &Value) -> Option<String> {
    let total = reflows["total_ms"].as_f64()?;
    if total <= FORCED_REFLOW_MS {
        return None;
    }
    let site = &reflows["top_call_sites"][0];
    let function = site["function"]
        .as_str()
        .filter(|f| !f.is_empty())
        .unwrap_or("(anonymous)");
    let at = format!(
        "{}:{}",
        site["url"].as_str().unwrap_or_default(),
        site["line"]
    );
    let mostly = if site.is_object() {
        format!(", mostly {function} ({at})")
    } else {
        String::new()
    };
    Some(format!(
        "forced reflows took {} ms in {} layouts or style recalculations{mostly}",
        shown(&reflows["total_ms"]),
        reflows["count"]
    ))
}

/// Unused JavaScript or CSS over [`UNUSED_CODE_BYTES`], with the file that
/// wastes most.
fn unused_code_findings(unused: &Value) -> Vec<String> {
    let kib = |bytes: u64| (bytes as f64 / 1024.0).round();
    let mut findings = Vec::new();
    for (kind, key, what) in [
        ("js", "unused_js_bytes", "of JavaScript never ran"),
        ("css", "unused_css_bytes", "of CSS matched nothing"),
    ] {
        let bytes = unused[key].as_u64().unwrap_or(0);
        if bytes <= UNUSED_CODE_BYTES {
            continue;
        }
        let first = unused["top_unused"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["kind"] == kind))
            .map(|r| {
                let url = r["url"].as_str().unwrap_or_default();
                let part = r["unused_bytes"].as_u64().unwrap_or(0);
                let whole = r["total_bytes"].as_u64().unwrap_or(0);
                format!(" (first: {url}, {} KiB of {} KiB)", kib(part), kib(whole))
            })
            .unwrap_or_default();
        findings.push(format!("{} KiB {what} during the load{first}", kib(bytes)));
    }
    findings
}

/// Why each `null` section is missing.
fn missing_notes(found: &Value, lcp_supported: bool, inp_unsupported: bool) -> Vec<String> {
    let lcp = if lcp_supported {
        "the page painted no largest-contentful-paint candidate yet"
    } else {
        "this browser does not report largest-contentful-paint"
    };
    let mut notes: Vec<String> = [
        ("document", "the page has no navigation timing"),
        ("lcp", lcp),
        ("cls", "this browser does not report layout shifts"),
        ("render_blocking", "this browser does not say which requests block rendering (renderBlockingStatus)"),
        ("long_scripts", "this browser does not report long animation frames, which name the scripts of long tasks"),
    ]
    .into_iter()
    .filter(|(key, _)| found[*key].is_null())
    .map(|(key, why)| format!("{key} is null: {why}"))
    .collect();
    if inp_unsupported && found["inp"].is_null() {
        notes.push("inp is null: this browser does not report event timing".into());
    }
    notes
}

/// `value` as JSON prints it (`4200`, `0.3`).
fn shown(value: &Value) -> String {
    value.to_string()
}

/// The rating of `value` against web.dev's thresholds: `None` when good.
fn rating(value: f64, (needs_improvement, poor): (f64, f64)) -> Option<String> {
    if value > poor {
        Some(format!("poor, over {poor}"))
    } else if value > needs_improvement {
        Some(format!("needs improvement, over {needs_improvement}"))
    } else {
        None
    }
}

/// The phase in `from` that took longest (`", mostly its <name> (<ms> ms)"`).
fn longest_phase(from: &Value, phases: &[(&str, &str)]) -> String {
    let longest = phases
        .iter()
        .filter_map(|(key, name)| Some((from[*key].as_f64()?, *name, *key)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    longest
        .map(|(_, name, key)| format!(", mostly its {name} ({} ms)", shown(&from[key])))
        .unwrap_or_default()
}

/// A slow LCP, with its element and the phase that took longest.
fn lcp_finding(lcp: &Value) -> Option<String> {
    let rated = rating(lcp["time_ms"].as_f64()?, (2500.0, 4000.0))?;
    let element = lcp["element"].as_str().unwrap_or("an element");
    let phase = longest_phase(
        &lcp["breakdown"],
        &[
            ("ttfb_ms", "server response"),
            ("load_delay_ms", "load delay"),
            ("load_duration_ms", "load"),
            ("render_delay_ms", "render delay"),
        ],
    );
    Some(format!(
        "LCP is {} ms ({rated}): {element}{phase}",
        shown(&lcp["time_ms"])
    ))
}

/// An LCP image whose attributes delay its discovery (`loading="lazy"`).
fn lcp_discovery_finding(lcp: &Value) -> Option<String> {
    if lcp["discovery"]["lazy_loaded"].as_bool() != Some(true) {
        return None;
    }
    let element = lcp["element"].as_str().unwrap_or("img");
    Some(format!(
        "LCP image ({element}) has loading=\"lazy\", which delays its discovery"
    ))
}

/// A slow Interaction to Next Paint (INP), with the event, target and dominant phase.
fn inp_finding(inp: &Value) -> Option<String> {
    let rated = rating(inp["duration_ms"].as_f64()?, (200.0, 500.0))?;
    let event = inp["name"].as_str().unwrap_or("interaction");
    let target = inp["target"]
        .as_str()
        .map(|t| format!(" ({t})"))
        .unwrap_or_default();
    let phase = longest_phase(
        inp,
        &[
            ("input_delay_ms", "input delay"),
            ("processing_ms", "processing"),
            ("presentation_delay_ms", "presentation delay"),
        ],
    );
    Some(format!(
        "INP is {} ms ({rated}) on {event}{target}{phase}",
        shown(&inp["duration_ms"])
    ))
}

/// A large or deeply nested DOM tree (over 1 500 nodes or depth 32).
fn dom_finding(dom: &Value) -> Option<String> {
    let nodes = dom["nodes"].as_u64()?;
    let depth = dom["max_depth"].as_u64()?;
    if nodes <= 1500 && depth <= 32 {
        return None;
    }
    let widest = dom["widest_element"].as_str().unwrap_or("element");
    let children = dom["max_children"].as_u64().unwrap_or(0);
    Some(format!(
        "DOM has {nodes} nodes (max depth {depth}, widest element {widest} with {children} children)"
    ))
}

/// Images that are oversized or lack explicit `width`/`height` attributes.
fn image_finding(images: &Value) -> Option<String> {
    let list = images.as_array().filter(|a| !a.is_empty())?;
    let first = list[0]["element"].as_str().unwrap_or("img");
    Some(format!(
        "{} image(s) are oversized or lack width/height attributes (first: {first})",
        list.len()
    ))
}

/// A high CLS, with what its largest shift moved.
fn cls_finding(cls: &Value) -> Option<String> {
    let rated = rating(cls["score"].as_f64()?, (0.1, 0.25))?;
    let moved: Vec<&str> = cls["culprits"][0]["moved"]
        .as_array()
        .map(|m| m.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let what = if moved.is_empty() {
        String::new()
    } else {
        format!(": the largest shift moved {}", moved.join(", "))
    };
    Some(format!("CLS is {} ({rated}){what}", shown(&cls["score"])))
}

/// A slow server response for the document (over 600 ms, web.dev's TTFB
/// guidance minus the network).
fn server_finding(document: &Value) -> Option<String> {
    let server = document["server_ms"].as_f64()?;
    (server > 600.0).then(|| {
        format!(
            "the server took {} ms to answer the document",
            shown(&document["server_ms"])
        )
    })
}

/// A document over 10 KB sent without compression.
fn compression_finding(document: &Value) -> Option<String> {
    let encoded = document["encoded_body_size"].as_u64()?;
    let decoded = document["decoded_body_size"].as_u64()?;
    (encoded == decoded && decoded > 10_000)
        .then(|| format!("the document ({decoded} bytes) is sent uncompressed"))
}

/// Requests that held the first render back.
fn blocking_finding(blocking: &Value) -> Option<String> {
    let requests = blocking.as_array().filter(|r| !r.is_empty())?;
    let until = requests
        .iter()
        .filter_map(|r| r["end_ms"].as_f64())
        .fold(0.0, f64::max);
    Some(format!(
        "{} render-blocking request(s) delayed the first render until {until} ms",
        requests.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    /// ISSUE-56: forced reflows join the insights with a finding over
    /// 30 ms, naming the call site that forced most; without a trace they
    /// are null with a note.
    #[test]
    fn forced_reflows_add_a_finding_or_a_note() {
        let reflows = json!({"count": 400, "total_ms": 42.5, "top_call_sites": [
            {"function": "thrash", "url": "http://a/t.js", "line": 6, "count": 400, "total_ms": 42.5}]});
        let mut insights = json!({});
        add_forced_reflows(&mut insights, Ok(reflows.clone()));
        assert_eq!(insights["forced_reflows"], reflows);
        assert_eq!(
            insights["findings"],
            json!([
                "forced reflows took 42.5 ms in 400 layouts or style recalculations, mostly thrash (http://a/t.js:6)"
            ])
        );
        let mut light = json!({});
        add_forced_reflows(
            &mut light,
            Ok(json!({"count": 2, "total_ms": 1.0, "top_call_sites": []})),
        );
        assert!(light.get("findings").is_none(), "{light}");
        let mut none = json!({});
        add_forced_reflows(&mut none, Err("no trace".into()));
        assert_eq!(none["notes"], json!(["forced_reflows is null: no trace"]));
    }

    /// ISSUE-54: unused code joins the insights with a finding over 50 KB
    /// of JavaScript or CSS; without it, it is null with a note saying why.
    #[test]
    fn unused_code_adds_a_finding_or_a_note() {
        let unused = json!({"unused_js_bytes": 80_000, "unused_css_bytes": 20_000, "top_unused": [
            {"url": "http://a/app.js", "kind": "js", "total_bytes": 100_000, "unused_bytes": 80_000, "unused_percent": 80.0}]});
        let mut insights = json!({"findings": ["LCP is slow"]});
        add_unused_code(&mut insights, Ok(unused.clone()));
        assert_eq!(insights["unused_code"], unused);
        assert_eq!(
            insights["findings"],
            json!([
                "LCP is slow",
                "78 KiB of JavaScript never ran during the load (first: http://a/app.js, 78 KiB of 98 KiB)"
            ])
        );
        let mut quiet = json!({});
        add_unused_code(&mut quiet, Err("it needs action: 'reload'".into()));
        assert_eq!(quiet["unused_code"], Value::Null);
        assert_eq!(
            quiet["notes"],
            json!(["unused_code is null: it needs action: 'reload'"])
        );
        assert!(quiet.get("findings").is_none(), "{quiet}");
    }

    /// ISSUE-22: Chrome reports every section; nothing is noted missing.
    #[test]
    fn chrome_insights_are_complete() {
        let found = fixture(include_str!(
            "../../tests/fixtures/insights/chrome-wikipedia.json"
        ));
        let insights = annotate(found);
        assert!(insights.get("notes").is_none(), "{insights}");
        assert!(insights.get("lcp_supported").is_none(), "{insights}");
        assert_eq!(insights["lcp"]["element"], "p#mwSw");
        assert!(insights["render_blocking"].as_array().unwrap().len() <= 5);
        let findings = insights["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|f| f
                .as_str()
                .unwrap()
                .starts_with("2 render-blocking request(s)")),
            "{insights}"
        );
    }

    /// ISSUE-22: what Firefox does not report is null with a note saying
    /// why, never an empty list that reads as \"none found\".
    #[test]
    fn firefox_insights_note_what_is_missing() {
        let found = fixture(include_str!(
            "../../tests/fixtures/insights/firefox-wikipedia.json"
        ));
        let insights = annotate(found);
        assert_eq!(insights["lcp"]["breakdown"]["load_delay_ms"], 212);
        let notes: Vec<&str> = insights["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].starts_with("cls is null"), "{notes:?}");
        assert!(notes[1].starts_with("render_blocking is null"), "{notes:?}");
        assert!(notes[2].starts_with("long_scripts is null"), "{notes:?}");
    }

    /// ISSUE-22: findings from the web.dev thresholds.
    #[test]
    fn findings_follow_the_thresholds() {
        let slow = json!({
            "document": {"ttfb_ms": 900, "server_ms": 800, "redirect_ms": 0, "transfer_size": 52000,
                "encoded_body_size": 51700, "decoded_body_size": 51700},
            "lcp": {"time_ms": 4200, "element": "img#hero", "url": "http://a/hero.jpg",
                "breakdown": {"ttfb_ms": 900, "load_delay_ms": 100, "load_duration_ms": 2900, "render_delay_ms": 300}},
            "cls": {"score": 0.3, "culprits": [{"value": 0.3, "at_ms": 900, "moved": ["div#ad"]}]},
            "render_blocking": [], "long_scripts": [], "slowest_resources": [], "lcp_supported": true
        });
        let findings = annotate(slow)["findings"].clone();
        assert_eq!(
            findings,
            json!([
                "LCP is 4200 ms (poor, over 4000): img#hero, mostly its load (2900 ms)",
                "CLS is 0.3 (poor, over 0.25): the largest shift moved div#ad",
                "the server took 800 ms to answer the document",
                "the document (51700 bytes) is sent uncompressed"
            ])
        );
        let quiet = json!({"document": null, "lcp": null, "cls": {"score": 0, "culprits": []},
            "render_blocking": [], "long_scripts": [], "slowest_resources": [], "lcp_supported": true});
        let insights = annotate(quiet);
        assert!(insights.get("findings").is_none(), "{insights}");
        assert_eq!(
            insights["notes"],
            json!([
                "document is null: the page has no navigation timing",
                "lcp is null: the page painted no largest-contentful-paint candidate yet"
            ])
        );
    }

    /// ISSUE-53: insights include INP, LCP discovery, DOM size, oversized or
    /// unsized images and third-party origins, with findings when they cross
    /// the thresholds.
    #[test]
    fn inp_discovery_dom_images_and_third_parties_produce_findings() {
        assert!(
            SNIPPET.contains("const events = take('event');"),
            "{SNIPPET}"
        );
        assert!(
            SNIPPET.contains("dom_size: domSize, images, third_parties: thirdParties"),
            "{SNIPPET}"
        );
        let page = json!({
            "document": {"ttfb_ms": 50, "server_ms": 20, "redirect_ms": 0, "transfer_size": 1000,
                "encoded_body_size": 800, "decoded_body_size": 2000},
            "lcp": {"time_ms": 1200, "element": "img#hero", "url": "http://a/hero.jpg", "size": 50000,
                "breakdown": {"ttfb_ms": 50, "load_delay_ms": 200, "load_duration_ms": 800, "render_delay_ms": 150},
                "discovery": {"lazy_loaded": true, "fetch_priority": "auto"}},
            "inp": {"duration_ms": 340, "name": "click", "target": "button#buy",
                "input_delay_ms": 20, "processing_ms": 280, "presentation_delay_ms": 40},
            "cls": {"score": 0, "culprits": []},
            "render_blocking": [],
            "long_scripts": [],
            "slowest_resources": [],
            "dom_size": {"nodes": 1820, "max_depth": 35, "widest_element": "ul#feed", "max_children": 90},
            "images": [{"element": "img#banner", "url": "http://a/b.jpg", "rendered_px": [200, 100],
                "natural_px": [2000, 1000], "oversized": true, "missing_dimensions": true}],
            "third_parties": [{"origin": "https://cdn.example.com", "requests": 4, "transfer_size": 120000, "total_duration_ms": 410}],
            "lcp_supported": true,
            "inp_supported": true
        });
        let insights = annotate(page);
        assert!(insights.get("inp_supported").is_none(), "{insights}");
        assert_eq!(
            insights["third_parties"][0]["origin"],
            "https://cdn.example.com"
        );
        let findings = insights["findings"].clone();
        assert_eq!(
            findings,
            json!([
                "LCP image (img#hero) has loading=\"lazy\", which delays its discovery",
                "INP is 340 ms (needs improvement, over 200) on click (button#buy), mostly its processing (280 ms)",
                "DOM has 1820 nodes (max depth 35, widest element ul#feed with 90 children)",
                "1 image(s) are oversized or lack width/height attributes (first: img#banner)"
            ])
        );
    }
}
