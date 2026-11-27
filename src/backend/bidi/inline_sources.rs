//! Firefox's inline scripts within their document (#222). The debugger
//! gives an inline script's source as its whole document, the same text
//! for every inline script of the page; a thread's `sources` reply says
//! where each one sits in it, which the announcements do not. A search
//! reads each inline script within its span only: text outside every
//! script is in none, and a match in one script is listed once.

use serde_json::Value;
use std::collections::HashMap;

/// Where an inline script sits in its document's text, as Firefox gives
/// it (measured on Firefox 160): its 1-based start line, its 0-based start
/// column in characters (code points), and its length in UTF-16 units, a
/// `\r\n` counting once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InlineSpan {
    line: usize,
    column: usize,
    length: usize,
}

/// The spans of the inline scripts the threads' `sources` replies list,
/// by source actor.
pub(crate) fn inline_spans(replies: &[Value]) -> HashMap<String, InlineSpan> {
    let number = |source: &Value, key: &str| {
        source
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    let sources = replies
        .iter()
        .filter_map(|reply| reply["sources"].as_array())
        .flatten();
    sources
        .filter(|source| source["isInlineSource"].as_bool() == Some(true))
        .filter_map(|source| {
            let span = InlineSpan {
                line: number(source, "sourceStartLine")?,
                column: number(source, "sourceStartColumn")?,
                length: number(source, "sourceLength")?,
            };
            Some((source["actor"].as_str()?.to_string(), span))
        })
        .collect()
}

/// The text of the script `span` places in `document`, and where it
/// starts there (0-based line and column, as
/// [`crate::handler::find_line_column`] counts them); the whole document
/// from its start without a span, or one the document does not hold.
pub(crate) fn script_text<'a>(
    document: &'a str,
    span: Option<&InlineSpan>,
) -> (&'a str, (u32, u32)) {
    let bytes = span.and_then(|span| {
        let line = line_start(document, span.line.checked_sub(1)?)?;
        let start = advance(document, line, span.column, |_| 1)?;
        Some(start..advance(document, start, span.length, char::len_utf16)?)
    });
    match bytes {
        Some(bytes) => (
            &document[bytes.clone()],
            crate::handler::line_column_at(document, bytes.start),
        ),
        None => (document, (0, 0)),
    }
}

/// The byte where 0-based line `line` of `text` starts (`\n`, `\r\n` and
/// `\r` end a line).
fn line_start(text: &str, line: usize) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    let mut seen = 0;
    while seen < line {
        let (_, c) = chars.next()?;
        let ends = match c {
            '\r' => {
                chars.next_if(|&(_, next)| next == '\n');
                true
            }
            '\n' => true,
            _ => false,
        };
        seen += usize::from(ends);
    }
    Some(chars.peek().map_or(text.len(), |&(at, _)| at))
}

/// The byte `count` units after byte `from` of `text`, each character
/// counting `units` of it and a `\r\n` once; `None` past its end, or
/// within a character.
fn advance(text: &str, from: usize, count: usize, units: fn(char) -> usize) -> Option<usize> {
    let rest = text.get(from..)?;
    let mut chars = rest.char_indices().peekable();
    let mut counted = 0;
    while counted < count {
        let (_, c) = chars.next()?;
        if c == '\r' {
            chars.next_if(|&(_, next)| next == '\n');
        }
        counted += units(c);
    }
    (counted == count).then(|| from + chars.peek().map_or(rest.len(), |&(at, _)| at))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Firefox 160's `sources` reply and inline source of a page with text
    /// outside its scripts, four inline scripts, characters outside the
    /// BMP and `\r\n` line ends.
    fn captured() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/rdp/inline-sources.json"
        ))
        .unwrap()
    }

    /// #222: each inline script is read within its span of the document:
    /// its body only, placed where it starts in the document, after
    /// characters outside the BMP (2 UTF-16 units each) and `\r\n` lines.
    #[test]
    fn inline_scripts_are_read_within_their_span() {
        // Given the page's inline sources and its text
        let captured = captured();
        let spans = inline_spans(&[captured["sources"].clone()]);
        let document = captured["source"]["source"].as_str().unwrap();
        let script = |n: u32| {
            let actor = format!("server1.conn1.watcher22.process8//source{n}");
            script_text(document, spans.get(&actor))
        };
        // Then each is its body, where it starts in the document
        assert_eq!(spans.len(), 4);
        assert_eq!(script(18), ("var a = 'inlineNeedle';", (2, 8)));
        assert_eq!(
            script(19),
            ("var b = 2;\n var c = 'secondNeedle ñ';", (3, 25))
        );
        assert_eq!(
            script(20),
            ("var e = '😀'; var after = 'emojiNeedle';", (5, 8))
        );
        assert_eq!(
            script(21),
            ("var f = 'crlf';\r\n var g = 'crlfNeedle';", (6, 8))
        );
        // And without a span the whole document is read
        assert_eq!(script_text(document, None), (document, (0, 0)));
    }

    /// #222: a search of every inline script of the page finds text in its
    /// scripts only, each match once, where it is in the document (as
    /// Chrome reports an inline script's); without the spans, the matches
    /// repeated are dropped.
    #[test]
    fn a_search_finds_script_text_once() {
        use crate::handler::cdp_domains::debugger::search_scripts::{ScriptText, SearchReport};
        // Given the page's four inline sources, each the whole document
        let captured = captured();
        let document = captured["source"]["source"].as_str().unwrap();
        let actors: Vec<String> = (18..=21)
            .map(|n| format!("server1.conn1.watcher22.process8//source{n}"))
            .collect();
        let search = |query: &str, spans: &HashMap<String, InlineSpan>| {
            let mut report = SearchReport::default();
            for actor in &actors {
                let (source, start) = script_text(document, spans.get(actor));
                let hash = "http://127.0.0.1:8000/inline.html";
                let script = ScriptText {
                    id: actor,
                    hash,
                    source,
                    start,
                };
                report.add_source(query, &script);
            }
            report.drop_repeated_places();
            report.results
        };
        let spans = inline_spans(&[captured["sources"].clone()]);
        // When text outside the scripts and text in one are searched
        let html = search("htmlOnlyNeedle", &spans);
        let found = search("emojiNeedle", &spans);
        // Then the first is in none, the second once, in its script
        assert!(html.is_empty(), "{html:?}");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0]["scriptId"], actors[2]);
        assert_eq!(
            (&found[0]["lineNumber"], &found[0]["columnNumber"]),
            (&5.into(), &35.into())
        );
        // And without the spans a match is listed once
        assert_eq!(search("emojiNeedle", &HashMap::new()).len(), 1);
    }

    /// #222: a span the document does not hold (another document's, or
    /// past its end) reads the whole document; a source that is not
    /// inline, or lacks a field, has no span.
    #[test]
    fn spans_the_document_does_not_hold_read_it_whole() {
        let span = |line, column, length| InlineSpan {
            line,
            column,
            length,
        };
        let document = "<p>a</p>\n<script>x</script>";
        assert_eq!(script_text(document, Some(&span(2, 8, 1))), ("x", (1, 8)));
        assert_eq!(script_text(document, Some(&span(3, 0, 1))).1, (0, 0));
        assert_eq!(script_text(document, Some(&span(2, 8, 99))).1, (0, 0));
        assert_eq!(script_text(document, Some(&span(0, 0, 1))).1, (0, 0));
        let reply = serde_json::json!({"sources": [
            {"actor": "a", "isInlineSource": false, "sourceStartLine": 1, "sourceStartColumn": 0, "sourceLength": 3},
            {"actor": "b", "isInlineSource": true, "sourceStartLine": 1},
        ]});
        assert!(inline_spans(&[reply]).is_empty());
    }
}
