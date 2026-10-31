//! `profile_page_performance`'s `insights`: what explains the numbers,
//! read in the page with the Web Performance APIs, the same JavaScript on
//! every browser (Chrome's trace has no ready-made explanation). Sections a
//! browser does not report are `null`, with a note; a few findings are
//! derived from the web.dev thresholds.

use serde_json::{Value, json};

/// The snippet: `{document, lcp, cls, render_blocking, long_scripts,
/// slowest_resources, lcp_supported}`, at most 5 entries per list.
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
        const entry = lcps[lcps.length - 1], start = ttfb || 0;
        const res = entry.url ? resources.find(r => r.name === entry.url) : null;
        const loadStart = res ? (res.requestStart || res.startTime) : null, loadEnd = res ? res.responseEnd : null;
        lcp = { time_ms: ms(entry.startTime), element: describe(entry.element), url: url(entry.url), size: entry.size,
            breakdown: { ttfb_ms: ms(start), load_delay_ms: res ? ms(Math.max(0, loadStart - start)) : null,
                load_duration_ms: res ? ms(loadEnd - loadStart) : null,
                render_delay_ms: ms(entry.startTime - (res ? Math.max(loadEnd, start) : start)) } };
    }
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
    return { document: document_, lcp, cls, render_blocking: blocking, long_scripts: scripts, slowest_resources: slowest,
        lcp_supported: types.includes('largest-contentful-paint') };
})()"#;

/// The page's insights with notes for what the browser does not report,
/// and the findings they show.
pub(crate) fn annotate(mut found: Value) -> Value {
    let lcp_supported = found.get("lcp_supported") == Some(&Value::Bool(true));
    let notes = missing_notes(&found, lcp_supported);
    let findings: Vec<String> = [
        lcp_finding(&found["lcp"]),
        cls_finding(&found["cls"]),
        server_finding(&found["document"]),
        compression_finding(&found["document"]),
        blocking_finding(&found["render_blocking"]),
    ]
    .into_iter()
    .flatten()
    .collect();
    let Some(object) = found.as_object_mut() else {
        return found;
    };
    object.remove("lcp_supported");
    if !findings.is_empty() {
        object.insert("findings".into(), json!(findings));
    }
    if !notes.is_empty() {
        object.insert("notes".into(), json!(notes));
    }
    found
}

/// Why each `null` section is missing.
fn missing_notes(found: &Value, lcp_supported: bool) -> Vec<String> {
    let lcp = if lcp_supported {
        "the page painted no largest-contentful-paint candidate yet"
    } else {
        "this browser does not report largest-contentful-paint"
    };
    [
        ("document", "the page has no navigation timing"),
        ("lcp", lcp),
        ("cls", "this browser does not report layout shifts"),
        ("render_blocking", "this browser does not say which requests block rendering (renderBlockingStatus)"),
        ("long_scripts", "this browser does not report long animation frames, which name the scripts of long tasks"),
    ]
    .into_iter()
    .filter(|(key, _)| found[*key].is_null())
    .map(|(key, why)| format!("{key} is null: {why}"))
    .collect()
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

/// A slow LCP, with its element and the phase that took longest.
fn lcp_finding(lcp: &Value) -> Option<String> {
    let rated = rating(lcp["time_ms"].as_f64()?, (2500.0, 4000.0))?;
    let element = lcp["element"].as_str().unwrap_or("an element");
    let phases = [
        ("ttfb_ms", "server response"),
        ("load_delay_ms", "load delay"),
        ("load_duration_ms", "load"),
        ("render_delay_ms", "render delay"),
    ];
    let longest = phases
        .iter()
        .filter_map(|(key, name)| Some((lcp["breakdown"][*key].as_f64()?, *name, *key)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    let phase = longest
        .map(|(_, name, key)| format!(", mostly its {name} ({} ms)", shown(&lcp["breakdown"][key])))
        .unwrap_or_default();
    Some(format!(
        "LCP is {} ms ({rated}): {element}{phase}",
        shown(&lcp["time_ms"])
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
}
