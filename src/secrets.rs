//! `--secrets <file>`: values the agent should not read back. Every tool
//! result and error has them replaced by `<secret:NAME>`, and `fill_input`
//! and `fill_form` type the value of a `<secret:NAME>` they are given, so a
//! password can be typed without the agent ever seeing it.
//!
//! Values are masked as is and URL-encoded as a form, `encodeURIComponent`
//! or `encodeURI` encodes them (a GET form puts a typed password in the
//! URL), each also escaped inside JSON strings, nested a few deep (a page's
//! `JSON.stringify` in a tool's JSON reply), with the characters outside
//! ASCII kept or as `\uXXXX` escapes, and in the page's own JSON also `/`
//! as `\/` and `&<>'=` as `\u00XX`. A convenience, not a security
//! boundary: a page can transform a value (encode it otherwise, split it)
//! into something no longer masked, and images (screenshots, PDFs) are not
//! masked.

use rust_mcp_sdk::schema::{
    CallToolError, CallToolRequestParams, CallToolResult, ContentBlock, EmbeddedResourceResource,
};
use serde_json::Value;

/// Shortest value accepted: masking a shorter one would mangle unrelated output.
pub const MIN_LENGTH: usize = 4;

/// Largest secrets file read.
const MAX_FILE: u64 = 1_000_000;

/// The tools that type `<secret:NAME>` as the secret's value, and the
/// argument that carries the text (`fill_form`: each field's `value`).
const TYPING_TOOLS: &[&str] = &["fill_input", "fill_form"];

/// The secrets, by name. Never logged: `Debug` shows the names only.
#[derive(Default, Clone)]
pub struct Secrets {
    /// `(name, value)`, in the file's order.
    entries: Vec<(String, String)>,
    /// Every written form of the values with its token, the longest first
    /// (see [`forms_of`]): built once, since `mask` runs on every result.
    forms: Vec<(String, String)>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.entries.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("Secrets").field("names", &names).finish()
    }
}

impl Secrets {
    /// Reads a dotenv file. Errors name the line, never a value.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let shown = path.display();
        let size = std::fs::metadata(path)
            .map_err(|e| format!("cannot read the secrets file '{shown}': {e}"))?
            .len();
        if size > MAX_FILE {
            return Err(format!(
                "the secrets file '{shown}' has {size} bytes; at most {MAX_FILE}"
            ));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the secrets file '{shown}': {e}"))?;
        Self::parse(&text).map_err(|e| format!("secrets file '{shown}': {e}"))
    }

    /// Parses dotenv text: `NAME=value` lines, `#` comments, an optional
    /// `export ` prefix, values optionally in single or double quotes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let n = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").map_or(line, str::trim_start);
            let Some((name, value)) = line.split_once('=') else {
                return Err(format!("line {n}: expected NAME=value"));
            };
            let name = name.trim();
            if !valid_name(name) {
                return Err(format!(
                    "line {n}: '{name}' is not a valid name (letters, digits and _, not starting with a digit)"
                ));
            }
            let value = unquote(value.trim());
            if value.chars().count() < MIN_LENGTH {
                return Err(format!(
                    "line {n}: {name} is shorter than {MIN_LENGTH} characters (masking it would mangle unrelated output)"
                ));
            }
            if entries.iter().any(|(known, _)| known == name) {
                return Err(format!("line {n}: {name} is defined twice"));
            }
            entries.push((name.to_string(), value));
        }
        let forms = forms_of(&entries);
        Ok(Self { entries, forms })
    }

    /// Whether no secret is configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `text` with every secret value (as is or URL-encoded as the browser
    /// does, each also escaped inside nested JSON strings, its characters
    /// outside ASCII kept or as `\uXXXX`) replaced by `<secret:NAME>`.
    pub fn mask(&self, text: &str) -> String {
        let forms = &self.forms;
        if !forms.iter().any(|(form, _)| text.contains(form.as_str())) {
            return text.to_string();
        }
        // One pass over the original text, so a token already put in is
        // never matched again.
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'scan: while let Some(c) = rest.chars().next() {
            for (form, token) in forms {
                if let Some(after) = rest.strip_prefix(form.as_str()) {
                    out.push_str(token);
                    rest = after;
                    continue 'scan;
                }
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// `outcome` with the secrets masked in its text, its embedded text
    /// resources, its structured content and its error message.
    pub(crate) fn mask_outcome(
        &self,
        outcome: Result<CallToolResult, CallToolError>,
    ) -> Result<CallToolResult, CallToolError> {
        if self.is_empty() {
            return outcome;
        }
        match outcome {
            Ok(mut result) => {
                for block in &mut result.content {
                    self.mask_block(block);
                }
                for value in result
                    .structured_content
                    .iter_mut()
                    .flat_map(|m| m.values_mut())
                {
                    self.mask_value(value);
                }
                Ok(result)
            }
            Err(error) => {
                let message = error.to_string();
                let masked = self.mask(&message);
                if masked == message {
                    Err(error)
                } else {
                    Err(CallToolError::from_message(masked))
                }
            }
        }
    }

    fn mask_block(&self, block: &mut ContentBlock) {
        match block {
            ContentBlock::TextContent(content) => content.text = self.mask(&content.text),
            ContentBlock::EmbeddedResource(embedded) => {
                if let EmbeddedResourceResource::TextResourceContents(contents) =
                    &mut embedded.resource
                {
                    contents.text = self.mask(&contents.text);
                }
            }
            _ => {}
        }
    }

    fn mask_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.mask(text),
            Value::Array(items) => items.iter_mut().for_each(|v| self.mask_value(v)),
            Value::Object(map) => map.values_mut().for_each(|v| self.mask_value(v)),
            _ => {}
        }
    }

    /// `params` with `<secret:NAME>` replaced by the value in the text the
    /// typing tools type; an unknown name fails.
    pub(crate) fn reveal(
        &self,
        mut params: CallToolRequestParams,
    ) -> Result<CallToolRequestParams, CallToolError> {
        if !TYPING_TOOLS.contains(&params.name.as_str()) {
            return Ok(params);
        }
        let fill_form = params.name == "fill_form";
        let Some(arguments) = params.arguments.as_mut() else {
            return Ok(params);
        };
        let typed: Vec<&mut Value> = if fill_form {
            let fields = arguments.get_mut("fields").and_then(Value::as_array_mut);
            let fields = fields.into_iter().flatten();
            fields.filter_map(|field| field.get_mut("value")).collect()
        } else {
            arguments.get_mut("text").into_iter().collect()
        };
        for value in typed {
            if let Value::String(text) = value {
                *text = self.revealed(text).map_err(CallToolError::from_message)?;
            }
        }
        Ok(params)
    }

    /// `text` with each `<secret:NAME>` replaced by the value of NAME.
    fn revealed(&self, text: &str) -> Result<String, String> {
        const OPEN: &str = "<secret:";
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let name = after.find('>').map(|end| &after[..end]);
            let Some(name) = name.filter(|n| valid_name(n)) else {
                out.push_str(&rest[..start + OPEN.len()]);
                rest = after;
                continue;
            };
            out.push_str(&rest[..start]);
            out.push_str(self.value_of(name)?);
            rest = &after[name.len() + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn value_of(&self, name: &str) -> Result<&str, String> {
        if self.is_empty() {
            return Err(format!(
                "'<secret:{name}>' needs the server started with --secrets <file>"
            ));
        }
        let names: Vec<&str> = self.entries.iter().map(|(n, _)| n.as_str()).collect();
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| {
                format!(
                    "no secret named {name}; the secrets are: {}",
                    names.join(", ")
                )
            })
    }
}

/// How many JSON strings deep a value is masked: a tool's JSON reply of
/// a page's `JSON.stringify` of `JSON.stringify` is three (#118), plus one.
const JSON_LEVELS: usize = 4;

/// Each value with its token, as is and as the browser URL-encodes it
/// (#109), each also as escaped inside JSON strings nested up to
/// [`JSON_LEVELS`] deep (#118), by an encoder that keeps the characters
/// outside ASCII or one that escapes them (#126): the longest first, so a
/// value containing another is masked whole.
fn forms_of(entries: &[(String, String)]) -> Vec<(String, String)> {
    let mut forms = Vec::new();
    for (name, value) in entries {
        let token = format!("<secret:{name}>");
        let mut written = vec![value.clone()];
        for encoded in url_encoded(value) {
            written.push(lowercase_hex(&encoded));
            written.push(encoded);
        }
        let mut level = written.clone();
        for depth in 0..JSON_LEVELS {
            // The page's own encoder writes the innermost string; the levels
            // around it are `JSON.stringify` or a tool's (serde) reply.
            let page = depth == 0;
            level = level
                .iter()
                .flat_map(|form| json_forms(form, page))
                .collect();
            written.extend(level.iter().cloned());
        }
        written.sort();
        written.dedup();
        forms.extend(written.into_iter().map(|form| (form, token.clone())));
    }
    forms.sort_by_key(|(form, _)| std::cmp::Reverse(form.len()));
    forms
}

/// `text` as written inside a JSON string, without the quotes JSON adds
/// (the text's own may start or end it).
fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    let inner = quoted.strip_prefix('"').and_then(|q| q.strip_suffix('"'));
    inner.unwrap_or(text).to_string()
}

/// The ASCII characters JSON encoders write as `\u00XX` escapes, and in
/// which case: Go's `encoding/json` (`&<>`), Gson (`&<>'=`), and PHP's
/// `JSON_HEX_TAG | JSON_HEX_AMP | JSON_HEX_APOS` (uppercase hex) (#129).
const HEX_ESCAPED: [(&str, bool); 3] = [("&<>", false), ("&<>'=", false), ("&<>'", true)];

/// `text` as JSON encoders write it inside a string: with its characters
/// outside ASCII kept (`JSON.stringify`, serde) and, when it has any, with
/// them escaped (#126: Python's `json.dumps` writes ASCII only), the hex
/// digits in lowercase or in uppercase. With `other_encoders`, also as
/// the encoders that escape `/` as `\/` (PHP) or ASCII characters in
/// [`HEX_ESCAPED`] write it (#129).
fn json_forms(text: &str, other_encoders: bool) -> Vec<String> {
    let escaped = json_escaped(text);
    let mut ascii_escapes = vec![escaped.clone()];
    if other_encoders {
        for (set, uppercase) in HEX_ESCAPED {
            ascii_escapes.push(hex_escaped(&escaped, |c| set.contains(c), uppercase));
        }
        // `/` is never part of an escape serde writes.
        let slashed: Vec<String> = ascii_escapes
            .iter()
            .map(|form| form.replace('/', "\\/"))
            .collect();
        ascii_escapes.extend(slashed);
    }
    let mut forms = Vec::new();
    for form in ascii_escapes {
        if !form.is_ascii() {
            forms.push(hex_escaped(&form, |c| !c.is_ascii(), false));
            forms.push(hex_escaped(&form, |c| !c.is_ascii(), true));
        }
        forms.push(form);
    }
    forms.sort();
    forms.dedup();
    forms
}

/// `text` with each character `escaped` selects as the `\uXXXX` escapes of
/// its UTF-16 units: one, or a surrogate pair above U+FFFF.
fn hex_escaped(text: &str, escaped: impl Fn(char) -> bool, uppercase: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut units = [0; 2];
    for c in text.chars() {
        if !escaped(c) {
            out.push(c);
            continue;
        }
        for unit in c.encode_utf16(&mut units) {
            let hex = if uppercase {
                format!("{unit:04X}")
            } else {
                format!("{unit:04x}")
            };
            out.push_str("\\u");
            out.push_str(&hex);
        }
    }
    out
}

/// What `encodeURIComponent` escapes: all but ASCII letters, digits and
/// `-_.!~*'()`.
const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// What `encodeURI` escapes: also keeps the URL delimiters `;,/?:@&=+$#`.
const URI: &percent_encoding::AsciiSet = &URI_COMPONENT
    .remove(b';')
    .remove(b',')
    .remove(b'/')
    .remove(b'?')
    .remove(b':')
    .remove(b'@')
    .remove(b'&')
    .remove(b'=')
    .remove(b'+')
    .remove(b'$')
    .remove(b'#');

/// `value` as the browser URL-encodes it: as a form does (GET query, POST
/// body; `+` for a space), as `encodeURIComponent` and as `encodeURI` do,
/// with uppercase hex digits.
fn url_encoded(value: &str) -> [String; 3] {
    let form: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    let encode = |set| percent_encoding::utf8_percent_encode(value, set).to_string();
    [form, encode(URI_COMPONENT), encode(URI)]
}

/// `encoded` with the two hex digits after each `%` in lowercase.
fn lowercase_hex(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    let mut hex_left = 0;
    for c in encoded.chars() {
        if hex_left > 0 {
            out.push(c.to_ascii_lowercase());
            hex_left -= 1;
        } else {
            out.push(c);
            if c == '%' {
                hex_left = 2;
            }
        }
    }
    out
}

/// `NAME`: letters, digits and `_`, not starting with a digit.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A dotenv value: quotes removed, or an unquoted one cut at ` #` (a comment).
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    let uncommented = value.split(" #").next().unwrap_or(value);
    uncommented.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn secrets() -> Secrets {
        Secrets::parse("PASS=hunter22\nTOKEN=abc\"def\nLONG=hunter22-and-more\n").unwrap()
    }

    /// SECRETS-01: dotenv lines with comments, export, quotes and inline
    /// comments; errors name the line, never the value.
    #[test]
    fn dotenv_files_are_parsed() {
        let text =
            "# comment\n\nexport A_KEY=plain-value # note\nB=\"quoted # kept\"\nC='single'\n";
        let parsed = Secrets::parse(text).unwrap();
        assert_eq!(
            parsed.mask("plain-value quoted # kept single"),
            "<secret:A_KEY> <secret:B> <secret:C>"
        );
        assert_eq!(
            format!("{parsed:?}"),
            "Secrets { names: [\"A_KEY\", \"B\", \"C\"] }"
        );
        for (text, error) in [
            ("1BAD=longvalue", "line 1: '1BAD' is not a valid name"),
            ("no equals sign", "line 1: expected NAME=value"),
            ("\nSHORT=abc", "line 2: SHORT is shorter than 4 characters"),
            ("EMPTY=", "line 1: EMPTY is shorter than 4 characters"),
            ("A=longvalue\nA=othervalue", "line 2: A is defined twice"),
        ] {
            let message = Secrets::parse(text).unwrap_err();
            assert!(message.contains(error), "{message}");
            assert!(!message.contains("longvalue") && !message.contains("othervalue"));
        }
    }

    /// SECRETS-02: values are masked as is and as escaped in JSON, the
    /// longest first.
    #[test]
    fn values_are_masked() {
        let s = secrets();
        assert_eq!(
            s.mask("pw hunter22, more hunter22-and-more"),
            "pw <secret:PASS>, more <secret:LONG>"
        );
        let json = serde_json::to_string(&json!({"t": "abc\"def"})).unwrap();
        assert_eq!(s.mask(&json), "{\"t\":\"<secret:TOKEN>\"}");
        assert_eq!(Secrets::default().mask("hunter22"), "hunter22");
        // A value that starts or ends with a quote is masked whole in JSON
        let quoted = Secrets::parse("Q='\"abcd\"'").unwrap();
        let json = serde_json::to_string(&json!({"k": "\"abcd\""})).unwrap();
        assert_eq!(quoted.mask(&json), "{\"k\":\"<secret:Q>\"}");
    }

    /// #118: a value with a quote and a backslash is masked inside JSON
    /// strings nested in JSON strings (a tool's JSON reply of a page's
    /// `JSON.stringify`, even of `JSON.stringify`), and so are its
    /// URL-encoded forms.
    #[test]
    fn values_are_masked_in_nested_json_strings() {
        // Given a secret JSON escapes
        let s = Secrets::parse("PW=Qx\"y\\z7731\n").unwrap();
        let value = "Qx\"y\\z7731";
        // When the page stringifies it once or twice and the tool replies
        let once = serde_json::to_string(&json!({"pw": value})).unwrap();
        let twice = serde_json::to_string(&once).unwrap();
        for page in [once.clone(), twice] {
            let reply = serde_json::to_string_pretty(&json!({"value": page})).unwrap();
            // Then no form of the value is left
            let masked = s.mask(&reply);
            assert!(!masked.contains("z7731"), "{masked}");
            assert!(masked.contains("<secret:PW>"), "{masked}");
        }
        let reply = serde_json::to_string(&json!({"value": once})).unwrap();
        assert_eq!(s.mask(&reply), r#"{"value":"{\"pw\":\"<secret:PW>\"}"}"#);
        // And a URL-encoded form escaped in JSON is masked too
        let url = serde_json::to_string(&json!({"u": "?pw=Qx%22y%5Cz7731"})).unwrap();
        let reply = serde_json::to_string(&json!({"value": url})).unwrap();
        assert!(!s.mask(&reply).contains("z7731"));
    }

    /// SECRETS-05 (#109): a value is also masked as a form encodes it (a GET
    /// query or a POST body: `+` for a space) and as `encodeURIComponent`
    /// does, with upper- or lowercase hex digits.
    #[test]
    fn url_encoded_values_are_masked() {
        // Given a secret with characters both encodings change
        let s = Secrets::parse("PW=p@ss w&rd9~\n").unwrap();
        // When the text holds it encoded, Then it is masked whole
        assert_eq!(
            s.mask("/status/200?user=a&pass=p%40ss+w%26rd9%7E&x=1"),
            "/status/200?user=a&pass=<secret:PW>&x=1"
        );
        assert_eq!(s.mask("q=p%40ss%20w%26rd9~"), "q=<secret:PW>");
        assert_eq!(s.mask("q=p%40ss+w%26rd9%7e"), "q=<secret:PW>");
        assert_eq!(s.mask("q=p@ss%20w&rd9~"), "q=<secret:PW>"); // encodeURI
        let json = serde_json::to_string(&json!({"url": "?pass=p%40ss+w%26rd9%7E"})).unwrap();
        assert_eq!(s.mask(&json), "{\"url\":\"?pass=<secret:PW>\"}");
        // And a value with nothing to encode is masked as before
        assert_eq!(secrets().mask("pw=hunter22"), "pw=<secret:PASS>");
    }

    /// #126: a value with characters outside ASCII is masked as a JSON
    /// encoder that writes ASCII only escapes it (`\uXXXX` per UTF-16 unit,
    /// with lower- or uppercase hex digits; Python's `json.dumps`), also
    /// inside a tool's JSON reply, where the backslashes are doubled.
    #[test]
    fn non_ascii_values_are_masked_as_json_unicode_escapes() {
        // Given secrets with characters outside ASCII, one above U+FFFF
        let s = Secrets::parse("QA_TOKEN=Žluťoučký7731\nASTRAL=key😀\"7731\n").unwrap();
        // When a page echoes one as an ASCII-only JSON encoder writes it
        let body = r#"{"p": "\u017dlu\u0165ou\u010dk\u00fd7731"}"#;
        // Then it is masked, as text and inside the tool's JSON reply
        assert_eq!(s.mask(body), r#"{"p": "<secret:QA_TOKEN>"}"#);
        let reply = serde_json::to_string(&json!({"value": body})).unwrap();
        assert!(reply.contains(r"\\u017dlu"), "{reply}");
        assert_eq!(
            s.mask(&reply),
            r#"{"value":"{\"p\": \"<secret:QA_TOKEN>\"}"}"#
        );
        // And with uppercase hex digits, and as a surrogate pair
        let upper = r"\u017Dlu\u0165ou\u010Dk\u00FD7731";
        assert_eq!(s.mask(upper), "<secret:QA_TOKEN>");
        assert_eq!(s.mask(r#"key\ud83d\ude00\"7731"#), "<secret:ASTRAL>");
        assert_eq!(s.mask(r#"key\uD83D\uDE00\"7731"#), "<secret:ASTRAL>");
        // And the value as is still is, with what JSON escapes of it
        assert_eq!(s.mask("Žluťoučký7731"), "<secret:QA_TOKEN>");
        assert_eq!(s.mask(r#"key😀\"7731"#), "<secret:ASTRAL>");
    }

    /// #129: an ASCII value is masked as JSON encoders other than serde and
    /// `JSON.stringify` write it: `/` as `\/` (PHP), and `&<>` (Go), also
    /// `'=` (Gson), as `\u00XX` escapes, lower- or uppercase (PHP's
    /// `JSON_HEX_*`), also inside a tool's JSON reply.
    #[test]
    fn ascii_values_are_masked_as_other_json_encoders_escape_them() {
        // Given a secret with characters other encoders escape
        let s = Secrets::parse("QA_KEY=ab/cd&ef7731\nTAG=<a href='x=1'>7731\n").unwrap();
        // When a page returns it as PHP and as Go write it
        let body = r#"{"php":"ab\/cd&ef7731","go":"ab/cd\u0026ef7731"}"#;
        // Then both are masked, as text and inside the tool's JSON reply
        let masked = r#"{"php":"<secret:QA_KEY>","go":"<secret:QA_KEY>"}"#;
        assert_eq!(s.mask(body), masked);
        let reply = serde_json::to_string(&json!({"value": body})).unwrap();
        assert!(!s.mask(&reply).contains("ef7731"), "{}", s.mask(&reply));
        // And with both escapes at once, or as Gson and PHP's flags write them
        assert_eq!(s.mask(r"ab\/cd\u0026ef7731"), "<secret:QA_KEY>");
        let gson = r"\u003ca href\u003d\u0027x\u003d1\u0027\u003e7731";
        let php = r"\u003Ca href=\u0027x=1\u0027\u003E7731";
        let go = r"\u003ca href='x=1'\u003e7731";
        for form in [gson, php, go] {
            assert_eq!(s.mask(form), "<secret:TAG>", "{form}");
        }
    }

    /// SECRETS-03: a tool's text, embedded text, structured content and
    /// error are masked.
    #[test]
    fn outcomes_are_masked() {
        let s = secrets();
        let mut result = CallToolResult::text_content(vec!["typed hunter22".to_string().into()]);
        result.structured_content = Some(json!({"v": ["hunter22"]}).as_object().unwrap().clone());
        let masked = s.mask_outcome(Ok(result)).unwrap();
        let text = masked.content[0].as_text_content().unwrap().text.clone();
        assert_eq!(text, "typed <secret:PASS>");
        assert_eq!(masked.structured_content.unwrap()["v"][0], "<secret:PASS>");
        let error = s.mask_outcome(Err(CallToolError::from_message("threw hunter22")));
        assert_eq!(error.unwrap_err().to_string(), "threw <secret:PASS>");
    }

    /// SECRETS-04: fill_input and fill_form type a <secret:NAME>'s value;
    /// an unknown name fails naming the known ones; other tools are untouched.
    #[test]
    fn typing_tools_reveal_secrets() {
        let s = secrets();
        let call = |name: &str, args: Value| -> CallToolRequestParams {
            serde_json::from_value(json!({"name": name, "arguments": args})).unwrap()
        };
        let revealed = s
            .reveal(call(
                "fill_input",
                json!({"selector": "#p", "text": "x<secret:PASS>y"}),
            ))
            .unwrap();
        assert_eq!(revealed.arguments.unwrap()["text"], "xhunter22y");
        let fields = json!([{"selector": "#a", "value": "<secret:TOKEN>"}, {"selector": "#b", "value": "plain"}]);
        let revealed = s
            .reveal(call("fill_form", json!({"fields": fields})))
            .unwrap();
        let fields = &revealed.arguments.unwrap()["fields"];
        assert_eq!(
            (fields[0]["value"].as_str(), fields[1]["value"].as_str()),
            (Some("abc\"def"), Some("plain"))
        );
        let unknown = s
            .reveal(call(
                "fill_input",
                json!({"selector": "#p", "text": "<secret:NOPE>"}),
            ))
            .unwrap_err()
            .to_string();
        assert!(
            unknown.contains("NOPE") && unknown.contains("PASS"),
            "{unknown}"
        );
        let other = s
            .reveal(call(
                "evaluate_js",
                json!({"expression": "'<secret:PASS>'"}),
            ))
            .unwrap();
        assert_eq!(other.arguments.unwrap()["expression"], "'<secret:PASS>'");
        let none = Secrets::default()
            .reveal(call(
                "fill_input",
                json!({"selector": "#p", "text": "<secret:PASS>"}),
            ))
            .unwrap_err()
            .to_string();
        assert!(none.contains("--secrets"), "{none}");
    }
}
