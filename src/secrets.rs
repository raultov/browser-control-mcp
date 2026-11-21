//! `--secrets <file>`: values the agent should not read back. Every tool
//! result and error has them replaced by `<secret:NAME>`, and `fill_input`
//! and `fill_form` type the value of a `<secret:NAME>` they are given, so a
//! password can be typed without the agent ever seeing it.
//!
//! A value is found as is and under up to six layers of the encodings
//! pages, servers and the tools' replies write, nested in any order: JSON
//! and JavaScript string escapes (`\"`, `\\`, `\/`, `\'`, `\n`,
//! `\uXXXX` in either case and as surrogate pairs, `\u{...}`, `\xXX`), HTML
//! character references (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`,
//! `&nbsp;`, decimal and hexadecimal) and percent-encoding (`%XX` as UTF-8
//! in either case, as `escape()` writes Latin-1 and `%uXXXX`, `+` for a
//! space). The text is decoded layer by layer and each value found is
//! replaced where it was written: decoding is one function, where writing
//! every form of every value kept missing one and grew without bound
//! (#118-#160).
//!
//! A convenience, not a security boundary: it keeps known values from
//! reaching the agent by accident. Only a **whole** value is masked. Not
//! masked, by design: a part of a value (a result cut by a size limit, a
//! window the caller chose, `slice()`: the agent can ask for any part
//! anyway, and guessing parts next to a cut mark only moved the gap,
//! #181-#184), a value encoded otherwise (base64, a hash), transformed or
//! split by the page, or in an image (screenshots, PDFs).

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
    /// The values, decoded as the text is (see [`patterns`]), in one
    /// automaton with the token of each: built once, since `mask` runs on
    /// every result and scans each layer in one pass (#145).
    masker: Option<Masker>,
}

/// The patterns of the secrets and the token replacing each.
#[derive(Clone)]
struct Masker {
    /// Finds every occurrence of every pattern, overlapping ones too: two
    /// secrets that overlap in the text (`abcd7731` and `7731wxyz` in
    /// `abcd7731wxyz`) are both found, and [`replaced`] masks them as one
    /// stretch; a leftmost-first search left the second's end shown.
    forms: aho_corasick::AhoCorasick,
    /// What each form stands for, by the form's index.
    tokens: Vec<Token>,
    /// How far (bytes) a match may reach past an escape it touches: the
    /// longest form and a character [`widened`] adds (see
    /// [`Layer::windows`]).
    reach: usize,
}

/// What a pattern stands for: the token replacing it, and which part of
/// its value it is.
#[derive(Clone)]
struct Token {
    text: String,
    cut: Cut,
}

/// Which part of a value a pattern is. A value's first character may
/// complete an escape the text before it starts (a `\` before its `n`), and
/// its last ones may start an escape the text after it completes (its
/// trailing `\` before an attribute's closing `"`): decoded, the value is
/// then not whole, but the rest of it is, and the escape it shares with
/// its surroundings is masked with it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// The whole value.
    Whole,
    /// The value without its first character: the character before a
    /// match is masked with it.
    Start,
    /// The value without the escape start that ends it: the character
    /// after a match is masked with it.
    End,
}

impl Masker {
    fn new(forms: Vec<(String, Token)>) -> Result<Option<Self>, String> {
        if forms.is_empty() {
            return Ok(None);
        }
        let (texts, tokens): (Vec<String>, Vec<Token>) = forms.into_iter().unzip();
        let reach = texts.iter().map(String::len).max().unwrap_or(0) + 4;
        // Never a DFA: its 256 transitions a state take gigabytes for a
        // long value (#150).
        let forms = aho_corasick::AhoCorasick::builder()
            .match_kind(aho_corasick::MatchKind::Standard)
            .kind(Some(aho_corasick::AhoCorasickKind::ContiguousNFA))
            .build(&texts)
            .map_err(|e| format!("the secrets are too long to mask: {e}"))?;
        Ok(Some(Self {
            forms,
            tokens,
            reach,
        }))
    }

    /// The forms found in `text`, widened as their cut says (see [`Cut`]),
    /// with their token's index, in `text`'s bytes. In a layer `decoded`
    /// from the one before, only the matches that hold a character one of
    /// its escapes decoded to: any other is the same bytes as in the layer
    /// before, found there already (#191: 100 overlapping secrets made each
    /// of seven layers find and remap a million matches). A match inside
    /// the one kept before it is left out: it changes neither what is
    /// masked nor the token (see [`replaced`]).
    fn found_in(&self, text: &str, decoded: Option<&Layer>) -> Vec<(Range, usize)> {
        let windows = match decoded {
            None => std::iter::once(0..text.len()).collect(),
            Some(layer) => layer.windows(text, self.reach),
        };
        let mut found: Vec<(Range, usize)> = Vec::new();
        for window in windows {
            let slice = text.get(window.clone()).unwrap_or_default();
            // Standard matching (see `Masker::new`) always allows it.
            let matches = self.forms.try_find_overlapping_iter(slice);
            let ranges = matches.into_iter().flatten().map(|value| {
                let token = value.pattern().as_usize();
                let cut = self.tokens.get(token).map_or(Cut::Whole, |t| t.cut);
                let at = window.start + value.start()..window.start + value.end();
                (widened(text, at, cut), token)
            });
            for (range, token) in ranges.filter(|(r, _)| decoded.is_none_or(|l| l.touches(r))) {
                let inside = found
                    .last()
                    .is_some_and(|(kept, _)| kept.start <= range.start && range.end <= kept.end);
                if !inside {
                    found.push((range, token));
                }
            }
        }
        found
    }
}

/// A stretch of a text, in bytes.
type Range = std::ops::Range<usize>;

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
        let masker = Masker::new(patterns(&entries))?;
        Ok(Self { entries, masker })
    }

    /// Whether no secret is configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `text` with every secret value, as is or encoded (see the module),
    /// replaced by `<secret:NAME>` where it was written.
    pub fn mask(&self, text: &str) -> String {
        let Some(masker) = &self.masker else {
            return text.to_string();
        };
        let mut found = Vec::new();
        let mut layers: Vec<Layer> = Vec::new();
        let mut current = std::borrow::Cow::Borrowed(text);
        for layer in 0..=LAYERS {
            for (range, token) in masker.found_in(&current, layers.last()) {
                let range = layers
                    .iter()
                    .rev()
                    .fold(range, |range, layer| layer.source(range));
                found.push((range, token));
            }
            if layer == LAYERS {
                break;
            }
            let Some((next, map)) = decoded(&current) else {
                break;
            };
            layers.push(map);
            current = std::borrow::Cow::Owned(next);
        }
        replaced(text, found, &masker.tokens)
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

/// How many layers of escapes are decoded: a tool's JSON reply of a page's
/// `JSON.stringify` of `JSON.stringify` of a URL-encoded value is four.
const LAYERS: usize = 6;

/// The patterns the masker looks for, each with its token: every secret's
/// value, and its [`cuts`], decoded 0 to [`LAYERS`] times (a value holding
/// what reads as an escape, `%41` or `&amp;`, is decoded in the text as
/// well), each also with its spaces as `+` (a form's encoding). Patterns
/// shorter than [`MIN_LENGTH`] are left out: they would mangle unrelated
/// output.
fn patterns(entries: &[(String, String)]) -> Vec<(String, Token)> {
    let mut patterns = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (name, value) in entries {
        for (form, cut) in cuts(value) {
            let token = Token {
                text: format!("<secret:{name}>"),
                cut,
            };
            let variants = decodings(form)
                .into_iter()
                .flat_map(|f| [f.replace(' ', "+"), f]);
            for variant in variants.filter(|v| v.chars().count() >= MIN_LENGTH) {
                if seen.insert(variant.clone()) {
                    patterns.push((variant, token.clone()));
                }
            }
        }
    }
    patterns
}

/// `form`, then it decoded once, twice... up to [`LAYERS`] times, while
/// decoding changes it.
fn decodings(form: String) -> Vec<String> {
    let mut forms = vec![form];
    while forms.len() <= LAYERS {
        let Some((next, _)) = forms.last().and_then(|last| decoded(last)) else {
            break;
        };
        forms.push(next);
    }
    forms
}

/// `value`, and the parts of it that stay whole when it shares an escape
/// with the text around it (see [`Cut`]): without its first character
/// when that character completes a script escape (`"'\/bfnrtuvx`), and
/// without each end that starts an escape (from a `\`, `%` or `&` among its
/// last 9 characters).
fn cuts(value: &str) -> Vec<(String, Cut)> {
    let mut cuts = vec![(value.to_string(), Cut::Whole)];
    let mut chars = value.chars();
    if chars.next().is_some_and(|c| "\"'\\/bfnrtuvx".contains(c)) {
        cuts.push((chars.as_str().to_string(), Cut::Start));
    }
    let ends = value.char_indices().rev().take(9);
    for (at, _) in ends.filter(|(at, c)| *at > 0 && matches!(c, '\\' | '%' | '&')) {
        cuts.push((value[..at].to_string(), Cut::End));
    }
    cuts
}

/// `range` of `text` widened as `cut` says: by the character before it
/// ([`Cut::Start`]) or after it ([`Cut::End`]), when there is one.
fn widened(text: &str, range: std::ops::Range<usize>, cut: Cut) -> std::ops::Range<usize> {
    match cut {
        Cut::Whole => range,
        Cut::Start => {
            let before = text.get(..range.start).and_then(|t| t.chars().next_back());
            range.start - before.map_or(0, char::len_utf8)..range.end
        }
        Cut::End => {
            let after = text.get(range.end..).and_then(|t| t.chars().next());
            range.start..range.end + after.map_or(0, char::len_utf8)
        }
    }
}

/// Where each character of a text with one layer of escapes decoded came
/// from in the text decoded.
struct Layer {
    /// The escapes decoded, in order; the text between them is copied as
    /// is.
    escapes: Vec<Escape>,
}

/// One escape: the character it decoded to at `out` (bytes) of the
/// decoded text, written at `source` (bytes) of the text decoded.
struct Escape {
    out: std::ops::Range<usize>,
    source: std::ops::Range<usize>,
}

impl Layer {
    /// The stretches of `text`, the text this layer decoded to, that hold
    /// every match touching one of its escapes: `reach` bytes around each
    /// escape, at character boundaries, overlapping ones joined. A layer
    /// that decoded 15 characters is searched around them, not whole.
    fn windows(&self, text: &str, reach: usize) -> Vec<Range> {
        let mut windows: Vec<Range> = Vec::new();
        for escape in &self.escapes {
            let start = text.floor_char_boundary(escape.out.start.saturating_sub(reach));
            let end = text.ceil_char_boundary(escape.out.end.saturating_add(reach));
            match windows.last_mut() {
                Some(last) if start <= last.end => last.end = last.end.max(end),
                _ => windows.push(start..end),
            }
        }
        windows
    }

    /// Whether `range` of the decoded text holds a character one of the
    /// layer's escapes decoded to.
    fn touches(&self, range: &Range) -> bool {
        let next = self.escapes.partition_point(|e| e.out.end <= range.start);
        self.escapes
            .get(next)
            .is_some_and(|e| e.out.start < range.end)
    }

    /// The bytes of the text decoded that `range` of the decoded text
    /// came from.
    fn source(&self, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        let start = self.source_of(range.start, false);
        let end = match range.end {
            0 => 0,
            end => self.source_of(end - 1, true),
        };
        start..end.max(start)
    }

    /// Where byte `at` of the decoded text came from: its start, or with
    /// `after` its end.
    fn source_of(&self, at: usize, after: bool) -> usize {
        let index = self.escapes.partition_point(|e| e.out.start <= at);
        let Some(escape) = index.checked_sub(1).and_then(|i| self.escapes.get(i)) else {
            return at + usize::from(after);
        };
        if at < escape.out.end {
            return if after {
                escape.source.end
            } else {
                escape.source.start
            };
        }
        escape.source.end + (at - escape.out.end) + usize::from(after)
    }
}

/// `text` with one layer of escapes decoded, and where each character of
/// it came from: JSON and JavaScript string escapes, HTML character
/// references and percent-encoding, all in one left-to-right pass; `None`
/// when it has none.
fn decoded(text: &str) -> Option<(String, Layer)> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut escapes = Vec::new();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        let escape = match bytes[at] {
            b'\\' => script_escape(text, at),
            b'&' => character_reference(text, at),
            b'%' => percent_escape(text, at),
            _ => None,
        };
        let Some((character, length)) = escape else {
            at += 1;
            continue;
        };
        // `at` is on an ASCII byte: a character boundary.
        out.push_str(text.get(copied..at).unwrap_or_default());
        let start = out.len();
        out.push(character);
        escapes.push(Escape {
            out: start..out.len(),
            source: at..at + length,
        });
        at += length;
        copied = at;
    }
    if escapes.is_empty() {
        return None;
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Some((out, Layer { escapes }))
}

/// The hexadecimal number written by `digits`: 1 to 6 hexadecimal digits,
/// nothing else (no sign).
fn hex(digits: &str) -> Option<u32> {
    let valid = (1..=6).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit());
    valid
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

/// The JSON or JavaScript string escape at `at` (a `\`): the character
/// and how many bytes it takes. `\uXXXX` pairs of surrogates are one
/// character; a lone surrogate, or an escape JSON and JavaScript do not
/// define, is not decoded.
fn script_escape(text: &str, at: usize) -> Option<(char, usize)> {
    let rest = text.get(at + 1..)?;
    let simple = match rest.as_bytes().first()? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'\'' => '\'',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'v' => '\u{b}',
        b'x' => return char::from_u32(hex(rest.get(1..3)?)?).map(|c| (c, 4)),
        b'u' => return unicode_escape(rest),
        _ => return None,
    };
    Some((simple, 2))
}

/// `\u{...}`, `\uXXXX` or a pair of `\uXXXX` surrogates, after its `\`.
fn unicode_escape(rest: &str) -> Option<(char, usize)> {
    if let Some(braced) = rest.strip_prefix("u{") {
        let digits = &braced[..braced.find('}')?];
        return char::from_u32(hex(digits)?).map(|c| (c, digits.len() + 4));
    }
    utf16_escape(rest.get(1..)?, "\\u")
}

/// The character of the UTF-16 unit written by the 4 hexadecimal digits
/// at the start of `units`, after an escape of 2 bytes whose next one is
/// written `prefix` (`\u`, `%u`): a surrogate needs the next one, and the
/// two are one character. With the length of the escapes.
fn utf16_escape(units: &str, prefix: &str) -> Option<(char, usize)> {
    let unit = hex(units.get(..4)?)?;
    if !(0xD800..0xDC00).contains(&unit) {
        return char::from_u32(unit).map(|c| (c, 6));
    }
    let low = hex(units.get(4..10)?.strip_prefix(prefix)?)?;
    if !(0xDC00..0xE000).contains(&low) {
        return None;
    }
    char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)).map(|c| (c, 12))
}

/// The named HTML character references the DOM and server-side escapers
/// write.
const ENTITIES: &[(&str, char)] = &[
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&quot;", '"'),
    ("&apos;", '\''),
    ("&nbsp;", '\u{a0}'),
];

/// The HTML character reference at `at` (a `&`): named (see
/// [`ENTITIES`]), decimal (`&#39;`) or hexadecimal (`&#x27;`).
fn character_reference(text: &str, at: usize) -> Option<(char, usize)> {
    let rest = text.get(at..)?;
    if let Some((name, c)) = ENTITIES.iter().find(|(name, _)| rest.starts_with(name)) {
        return Some((*c, name.len()));
    }
    let number = rest.strip_prefix("&#")?;
    let digits_end = number.find(';').filter(|end| *end <= 8)?;
    let digits = &number[..digits_end];
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex_digits) => hex(hex_digits)?,
        None if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()?
        }
        None => return None,
    };
    char::from_u32(code).map(|c| (c, digits_end + 3))
}

/// The percent-encoded character at `at` (a `%`): `%XX` bytes read as
/// UTF-8, one byte that starts no UTF-8 character read as Latin-1 (as
/// `escape()` writes `é`), or `%uXXXX` (`escape()` above U+00FF).
fn percent_escape(text: &str, at: usize) -> Option<(char, usize)> {
    let rest = text.get(at..)?;
    if let Some(units) = rest.strip_prefix("%u") {
        return utf16_escape(units, "%u");
    }
    let byte = |n: usize| -> Option<u8> {
        let digits = rest.get(n * 3..n * 3 + 3)?.strip_prefix('%')?;
        u8::try_from(hex(digits)?).ok()
    };
    let first = byte(0)?;
    let length = match first {
        0x00..=0x7F => return Some((char::from(first), 3)),
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    };
    let utf8: Option<Vec<u8>> = (0..length).map(byte).collect();
    let decoded = utf8.and_then(|b| String::from_utf8(b).ok());
    match decoded.and_then(|s| s.chars().next()) {
        Some(c) if length > 1 => Some((c, length * 3)),
        _ => Some((char::from(first), 3)),
    }
}

/// `text` with each range of `found` (with its token's index) replaced by
/// its token; overlapping ranges are replaced once, by the token of the
/// one that starts first (the longest of those).
fn replaced(
    text: &str,
    mut found: Vec<(std::ops::Range<usize>, usize)>,
    tokens: &[Token],
) -> String {
    found.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut merged: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
    for (range, token) in found {
        match merged.last_mut() {
            Some((open, _)) if range.start < open.end => open.end = open.end.max(range.end),
            _ => merged.push((range, token)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (range, token) in merged {
        out.push_str(text.get(copied..range.start).unwrap_or_default());
        out.push_str(tokens.get(token).map_or("<secret>", |t| t.text.as_str()));
        copied = range.end;
    }
    out.push_str(text.get(copied..).unwrap_or_default());
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

    /// #137: a value with `&`, `<`, `>` or quotes is masked as serialized
    /// HTML escapes it (`innerHTML`, `inspect_dom`), in a text node or an
    /// attribute, and as server-side escapers write its quotes, also inside
    /// a tool's JSON reply.
    #[test]
    fn values_are_masked_as_html_escapes_them() {
        // Given a secret with characters HTML escapes
        let s = Secrets::parse("QA_H='ab&c<d>e\"f\'8841'\n").unwrap();
        // When a page serializes it in a text node, an attribute, or an
        // escaper writes its quotes
        for html in [
            "<p id=\"s\">ab&amp;c&lt;d&gt;e\"f'8841</p>",
            "<input value=\"ab&amp;c<d>e&quot;f'8841\">",
            "<input value=\"ab&amp;c&lt;d&gt;e&quot;f'8841\">",
            "ab&amp;c&lt;d&gt;e&quot;f&#039;8841",
            "ab&amp;c&lt;d&gt;e&quot;f&#x27;8841",
            "ab&amp;c&lt;d&gt;e&quot;f&#39;8841",
        ] {
            // Then no form of the value is left, as text or in JSON
            let reply = serde_json::to_string(&json!({"value": html})).unwrap();
            for text in [html, reply.as_str()] {
                let masked = s.mask(text);
                assert!(!masked.contains("8841"), "{masked}");
                assert!(masked.contains("<secret:QA_H>"), "{masked}");
            }
        }
    }

    /// #148: a value inside a page's JSON held in an HTML attribute is
    /// masked as the DOM serializes it (`outerHTML`, `inspect_dom`): JSON
    /// escapes first, then HTML ones, also inside a tool's JSON reply.
    #[test]
    fn values_in_json_in_html_attributes_are_masked() {
        // Given a secret with a quote and an ampersand
        let s = Secrets::parse("QA_J='Qa\"Pw&rd7731'\n").unwrap();
        // When a page serializes an attribute holding JSON with it
        let html =
            r#"<div id="c" data-config="{&quot;token&quot;:&quot;Qa\&quot;Pw&amp;rd7731&quot;}">"#;
        let reply = serde_json::to_string(&json!({"value": html})).unwrap();
        // Then it is masked, as text and in the tool's JSON reply
        let masked = s.mask(html);
        assert_eq!(
            masked,
            r#"<div id="c" data-config="{&quot;token&quot;:&quot;<secret:QA_J>&quot;}">"#
        );
        assert!(!s.mask(&reply).contains("7731"), "{}", s.mask(&reply));
        // And as PHP's `htmlspecialchars(json_encode(...))` writes it
        assert_eq!(s.mask(r"Qa\&quot;Pw\u0026rd7731"), "<secret:QA_J>");
    }

    /// #160: a value in a page's `JSON.stringify(JSON.stringify(...))` held
    /// in an HTML attribute is masked as the DOM serializes it, also inside
    /// a tool's JSON reply.
    #[test]
    fn values_in_twice_stringified_json_in_html_attributes_are_masked() {
        // Given a secret with a quote, angle brackets and an ampersand
        let s = Secrets::parse("QA_TOKEN='qaZ7\"Xk2W<Vw9P>&mN4'\n").unwrap();
        // When a page serializes an attribute holding twice-stringified JSON with it
        let html = r#"<div id="cfg" data-twice="&quot;{\&quot;t\&quot;:\&quot;qaZ7\\\&quot;Xk2W&lt;Vw9P&gt;&amp;mN4\&quot;}&quot;"></div>"#;
        let reply = serde_json::to_string(&json!({"value": html})).unwrap();
        // Then it is masked, as text and in the tool's JSON reply
        let masked = s.mask(html);
        assert_eq!(
            masked,
            r#"<div id="cfg" data-twice="&quot;{\&quot;t\&quot;:\&quot;<secret:QA_TOKEN>\&quot;}&quot;"></div>"#
        );
        let masked = s.mask(&reply);
        for part in ["Xk2W", "Vw9P", "mN4"] {
            assert!(!masked.contains(part), "{masked}");
        }
    }

    /// #150: a 32 KB symbol-heavy value is parsed fast and in little
    /// memory, and masked as is, in a tool's JSON reply and in JSON nested
    /// deeper; a short secret next to it keeps every encoded form.
    #[test]
    fn long_values_are_masked_in_every_form() {
        // Given a 32 000-character value of characters every encoding changes
        let long: String = "\"&<>/\u{e9}\\".repeat(4600).chars().take(32_000).collect();
        let value = format!("{long}END7319");
        let file = format!("BIG='{value}'\nPW=p@ss w&rd9~\n");
        // When the secrets are read
        let started = std::time::Instant::now();
        let s = Secrets::parse(&file).unwrap();
        // Then it is fast, and the value is masked in every form
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(s.mask(&format!("<{value}>")), "<<secret:BIG>>");
        let reply = serde_json::to_string(&json!({"value": value})).unwrap();
        assert_eq!(s.mask(&reply), r#"{"value":"<secret:BIG>"}"#);
        let twice = serde_json::to_string(&json!({"value": reply})).unwrap();
        assert_eq!(
            s.mask(&twice),
            r#"{"value":"{\"value\":\"<secret:BIG>\"}"}"#
        );
        // And the short secret keeps its rare forms
        assert_eq!(s.mask(r"p%40ss+w%26rd9%7e"), "<secret:PW>");
        assert_eq!(s.mask(r"p@ss w\u0026rd9~"), "<secret:PW>");
    }

    /// The encodings a probe of 16 found unmasked before: JSON nested four
    /// deep, JSON in a URL, a URL encoded twice, decimal and hexadecimal
    /// HTML references, a JavaScript single-quoted literal, HTML in a URL
    /// and `escape()`.
    #[test]
    fn deeper_and_mixed_encodings_are_masked() {
        let s = Secrets::parse("PW='p@ss\\'w&rd\"x\\9\u{e9}/z<k'\n").unwrap();
        let value = "p@ss'w&rd\"x\\9\u{e9}/z<k";
        assert_eq!(s.mask(value), "<secret:PW>");
        let json4 = (0..4).fold(value.to_string(), |text, _| json_string(&text));
        let url_json = percent(&format!("{{\"p\":{}}}", json_string(value)));
        for (name, text) in [
            ("JSON x4", json4),
            ("URL of JSON", url_json),
            ("URL twice", percent(&percent(value))),
            (
                "HTML decimal",
                "p@ss&#39;w&#38;rd&#34;x\\9\u{e9}/z&#60;k".to_string(),
            ),
            (
                "HTML hex",
                "p@ss&#x27;w&#x26;rd&#x22;x\\9\u{e9}/z&#x3c;k".to_string(),
            ),
            ("JS literal", "'p@ss\\'w&rd\"x\\\\9\u{e9}/z<k'".to_string()),
            (
                "URL of HTML",
                "p%40ss'w%26amp%3Brd%22x%5C9%C3%A9%2Fz%26lt%3Bk".to_string(),
            ),
            ("escape()", "p@ss%27w%26rd%22x%5C9%E9/z%3Ck".to_string()),
        ] {
            let masked = s.mask(&format!("[{text}]"));
            assert!(masked.contains("<secret:PW>"), "{name}: {masked}");
            assert!(
                !masked.contains("rd") && !masked.contains("z<k"),
                "{name}: {masked}"
            );
        }
    }

    /// Decoding keeps where each character came from: a value found in the
    /// decoded text is replaced where it was written, whole, and nothing
    /// around it is touched; a decoded value shorter than [`MIN_LENGTH`]
    /// is not looked for.
    #[test]
    fn decoded_values_are_replaced_where_they_were_written() {
        let s = Secrets::parse("A='a&b\"c'\nP=%41%42\n").unwrap();
        assert_eq!(
            s.mask(r#"x a&amp;b\"c y a%26b%22c z &#97;&#38;b&#34;c"#),
            "x <secret:A> y <secret:A> z <secret:A>"
        );
        assert_eq!(s.mask("AB ABAB %41%42"), "AB ABAB <secret:P>");
        // And text that only looks like an escape is left as it is
        let plain = r"C:\users\x &amp &#zz; %zz \u12 \ud83d";
        assert_eq!(s.mask(plain), plain);
    }

    /// An encoder of the property test, with its name.
    type Encoder = (&'static str, fn(&str) -> String);

    /// The encoders the property test composes: `JSON.stringify`, an
    /// ASCII-only JSON encoder, PHP's (`\/` and `\u00XX` for `&<>'`), HTML
    /// text and attribute escaping, decimal and hexadecimal references for
    /// every symbol, `encodeURIComponent` (upper- and lowercase hex), a form
    /// (`+` for a space), a JavaScript single-quoted literal and `escape()`.
    fn encoders() -> Vec<Encoder> {
        vec![
            ("json", json_string as fn(&str) -> String),
            ("json-ascii", |t| {
                hex_units(&json_string(t), |c| !c.is_ascii())
            }),
            ("json-php", |t| {
                hex_units(&json_string(t).replace('/', "\\/"), |c| "&<>'".contains(c))
            }),
            ("html-text", |t| {
                t.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            }),
            ("html-attr", |t| {
                t.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;")
            }),
            ("html-dec", |t| references(t, |n| format!("&#{n};"))),
            ("html-hex", |t| references(t, |n| format!("&#x{n:x};"))),
            ("uri-component", percent),
            ("uri-lower", |t| lowercase_hex(&percent(t))),
            ("form", |t| {
                url::form_urlencoded::byte_serialize(t.as_bytes()).collect()
            }),
            ("js-single", |t| {
                format!(
                    "'{}'",
                    t.replace('\\', "\\\\")
                        .replace('\'', "\\'")
                        .replace('\n', "\\n")
                )
            }),
            ("escape", escape),
        ]
    }

    /// Property: any composition of up to three of [`encoders`] of a
    /// secret, written in the middle of a text, is masked, and the text
    /// around it is kept: values with symbols, outside ASCII (above U+FFFF
    /// too), with a space, with what reads as an escape (`%41`, `\u`), and
    /// a long one (compositions of up to two).
    #[test]
    fn every_composition_of_the_encodings_is_masked() {
        let long: String = "a\"&<>/\u{e9} \\%".repeat(400);
        let values = [
            ("SYM", "p@ss'w&rd\"x\\9\u{e9}/z<k".to_string(), 3),
            (
                "UNI",
                "\u{17d}lu\u{165}ou\u{10d}k\u{fd}\u{1f600}7731".to_string(),
                3,
            ),
            ("SPACE", "p@ss w&rd9~ q".to_string(), 3),
            ("PCT", "ab%41cd&x=1".to_string(), 3),
            ("BACK", "Q\\x\"y\\\\z\\u7".to_string(), 3),
            ("LONG", format!("{long}END"), 2),
        ];
        let encoders = encoders();
        assert_eq!(chains(encoders.len(), 3).len(), 12 + 12 * 12 + 12 * 12 * 12);
        for (name, value, depth) in &values {
            let s = Secrets::parse(&format!("{name}={}\n", quoted(value))).unwrap();
            let token = format!("<secret:{name}>");
            for chain in chains(encoders.len(), *depth) {
                let encoded = chain
                    .iter()
                    .fold(value.clone(), |text, &e| (encoders[e].1)(&text));
                let masked = s.mask(&format!("left [{encoded}] right"));
                let names: Vec<&str> = chain.iter().map(|&e| encoders[e].0).collect();
                assert!(
                    masked.starts_with("left [") && masked.ends_with("] right"),
                    "{names:?}: {masked}"
                );
                assert!(masked.contains(&token), "{name} {names:?}: {masked}");
                assert!(!masked.contains(&encoded), "{name} {names:?}: {masked}");
            }
        }
    }

    /// Every sequence of 1 to `depth` indexes below `count`.
    fn chains(count: usize, depth: usize) -> Vec<Vec<usize>> {
        let mut all: Vec<Vec<usize>> = (0..count).map(|e| vec![e]).collect();
        let mut last = all.clone();
        for _ in 1..depth {
            last = last
                .iter()
                .flat_map(|chain| (0..count).map(move |e| [chain.clone(), vec![e]].concat()))
                .collect();
            all.extend(last.iter().cloned());
        }
        all
    }

    /// `value` as a dotenv value: in the quotes it does not hold.
    fn quoted(value: &str) -> String {
        if value.contains('\'') {
            format!("\"{value}\"")
        } else {
            format!("'{value}'")
        }
    }

    /// `text` inside a JSON string, without the quotes.
    fn json_string(text: &str) -> String {
        let quoted = serde_json::to_string(text).unwrap();
        quoted[1..quoted.len() - 1].to_string()
    }

    /// `text` with the characters `escaped` selects as `\uXXXX` escapes of
    /// their UTF-16 units.
    fn hex_units(text: &str, escaped: fn(char) -> bool) -> String {
        let mut units = [0; 2];
        text.chars()
            .map(|c| match escaped(c) {
                true => c
                    .encode_utf16(&mut units)
                    .iter()
                    .map(|u| format!("\\u{u:04x}"))
                    .collect(),
                false => c.to_string(),
            })
            .collect()
    }

    /// `text` with every ASCII symbol as a character reference `format`
    /// writes.
    fn references(text: &str, format: fn(u32) -> String) -> String {
        text.chars()
            .map(|c| match c.is_ascii_punctuation() {
                true => format(u32::from(c)),
                false => c.to_string(),
            })
            .collect()
    }

    /// `text` as `encodeURIComponent` writes it.
    fn percent(text: &str) -> String {
        const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
            .remove(b'-')
            .remove(b'_')
            .remove(b'.')
            .remove(b'!')
            .remove(b'~')
            .remove(b'*')
            .remove(b'\'')
            .remove(b'(')
            .remove(b')');
        percent_encoding::utf8_percent_encode(text, COMPONENT).to_string()
    }

    /// `encoded` with the two hexadecimal digits after each `%` in
    /// lowercase.
    fn lowercase_hex(encoded: &str) -> String {
        let mut hex_left = 0;
        encoded
            .chars()
            .map(|c| match (hex_left, c) {
                (0, '%') => {
                    hex_left = 2;
                    c
                }
                (0, _) => c,
                _ => {
                    hex_left -= 1;
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }

    /// `text` as `escape()` writes it: Latin-1 as `%XX`, above as `%uXXXX`.
    fn escape(text: &str) -> String {
        let mut units = [0; 2];
        text.chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '@' | '*' | '_' | '+' | '-' | '.' | '/' => {
                    c.to_string()
                }
                _ if u32::from(c) < 256 => format!("%{:02X}", u32::from(c)),
                _ => c
                    .encode_utf16(&mut units)
                    .iter()
                    .map(|u| format!("%u{u:04X}"))
                    .collect(),
            })
            .collect()
    }

    /// #145: a reply of 100 KB is masked in one pass whatever the number of
    /// secrets and of their forms: 100 symbol-heavy secrets take well
    /// under a second, not minutes.
    #[test]
    fn many_secrets_are_masked_in_one_pass() {
        // Given 100 secrets with many encoded forms each
        let file: String = (0..100)
            .map(|i| format!("S{i:03}='Pw{i:03}/\u{e4}&<>\"x= y'\n"))
            .collect();
        let s = Secrets::parse(&file).unwrap();
        // When a 99 KB reply holding one of them is masked
        let reply = format!("{}Pw050/\u{e4}&<>\"x= y", "x".repeat(99_000));
        let started = std::time::Instant::now();
        let masked = s.mask(&reply);
        // Then it is masked, fast
        assert!(masked.ends_with("x<secret:S050>"), "{}", &masked[99_000..]);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// A large reply dense in escapes of every kind, each layer decoding to
    /// another (six layers), is masked in a bounded time, with the value
    /// in it found.
    #[test]
    fn large_replies_dense_in_escapes_are_masked_quickly() {
        // Given 100 secrets and a 2 MB reply of nested escapes holding one
        let file: String = (0..100)
            .map(|i| format!("S{i:03}='Pw{i:03}&<\"x y'\n"))
            .collect();
        let s = Secrets::parse(&file).unwrap();
        let unit = r#"\\\\&amp;amp;%2525 &quot;\u0026#"#;
        let reply = format!(
            "{}Pw050&amp;&lt;&quot;x y",
            unit.repeat(2_000_000 / unit.len())
        );
        // When it is masked
        let started = std::time::Instant::now();
        let masked = s.mask(&reply);
        // Then the value is found, quickly
        assert!(
            masked.ends_with("#<secret:S050>"),
            "{}",
            &masked[masked.len() - 40..]
        );
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }

    /// A value is masked wherever it is: the whole text, its start, its
    /// end, twice in a row, a thousand times; between escapes of what
    /// surrounds it (`\"`, `&quot;`, `%22`).
    #[test]
    fn values_are_masked_wherever_they_are() {
        let s = Secrets::parse("PW=p@ss7731\n").unwrap();
        for (text, masked) in [
            ("p@ss7731", "<secret:PW>"),
            ("p@ss7731 end", "<secret:PW> end"),
            ("start p@ss7731", "start <secret:PW>"),
            ("p@ss7731p@ss7731", "<secret:PW><secret:PW>"),
            (r#"\"p@ss7731\""#, r#"\"<secret:PW>\""#),
            ("&quot;p@ss7731&quot;", "&quot;<secret:PW>&quot;"),
            ("%22p%40ss7731%22", "%22<secret:PW>%22"),
        ] {
            assert_eq!(s.mask(text), masked, "{text}");
        }
        let many = "p@ss7731, ".repeat(1000);
        assert_eq!(s.mask(&many), "<secret:PW>, ".repeat(1000));
    }

    /// Secrets that overlap: the longer one is masked when one holds the
    /// other; when they only overlap, the one that starts first names the
    /// whole stretch, so neither shows.
    #[test]
    fn overlapping_secrets_are_masked_together() {
        let s = Secrets::parse("A=abcd7731\nB=7731wxyz\nC=abcd7731wxyz-long\n").unwrap();
        assert_eq!(s.mask("abcd7731wxyz-long"), "<secret:C>");
        assert_eq!(s.mask("abcd7731wxyz"), "<secret:A>");
        assert_eq!(s.mask("xx7731wxyz"), "xx<secret:B>");
        // And encoded, the same
        assert_eq!(s.mask("abcd7731wxyz%2Dlong"), "<secret:C>");
    }

    /// Text that only looks like an escape is never changed, and nothing
    /// panics on it: cut, invalid, out of range or lone surrogate escapes
    /// of every kind, also at the very end of the text.
    #[test]
    fn malformed_escapes_are_left_as_they_are() {
        let s = Secrets::parse("PW=p@ss7731\n").unwrap();
        for text in [
            "\\",
            "\\u",
            "\\u12",
            "\\u{",
            "\\u{}",
            "\\u{110000}",
            "\\u{1234567}",
            "\\uD800",
            "\\uD800\\u0041",
            "\\uDC00",
            "\\x",
            "\\x4",
            "\\x4G",
            "\\u+123",
            "%",
            "%4",
            "%G1",
            "%+1",
            "%E9%",
            "%F0%9F%98",
            "%C3",
            "%u",
            "%uD83D",
            "%uD83D%u0041",
            "&",
            "&#",
            "&#;",
            "&#x;",
            "&#99999999;",
            "&#x110000;",
            "&#xD800;",
            "&amp",
            "&AMP;",
            "&#x+41;",
            "&#-1;",
            "a\\\\\\",
            "%%%",
            "&&&;;",
        ] {
            assert_eq!(s.mask(text), text, "{text:?}");
            let around = format!("{text}p@ss7731{text}");
            assert!(!s.mask(&around).contains("p@ss7731"), "{around:?}");
        }
    }

    /// Masking is idempotent, leaves a token already in the text alone,
    /// and is case-sensitive (a value in another case is another value).
    #[test]
    fn masking_twice_changes_nothing_more() {
        let s = Secrets::parse("PW=p@ss7731\nTOKEN='<secret:PW>x'\n").unwrap();
        for text in [
            "a p@ss7731 b",
            r#"{"v":"p%40ss7731"}"#,
            "<secret:PW> stays",
            "&lt;secret:PW&gt;x",
            "P@SS7731",
        ] {
            let once = s.mask(text);
            assert_eq!(s.mask(&once), once, "{text}");
        }
        assert_eq!(s.mask("P@SS7731"), "P@SS7731");
        assert_eq!(s.mask("<secret:PW> stays"), "<secret:PW> stays");
    }

    /// A value that reads as an escape is masked as written and as decoded
    /// (a page may decode it): `%41%42%43%44` hides `ABCD` too. A decoded
    /// form shorter than [`MIN_LENGTH`] is not looked for.
    #[test]
    fn values_that_read_as_escapes_are_masked_as_decoded_too() {
        let s = Secrets::parse("P=%41%42%43%44\nQ='&lt;&gt;'\n").unwrap();
        assert_eq!(s.mask("%41%42%43%44 ABCD"), "<secret:P> <secret:P>");
        assert_eq!(s.mask("&lt;&gt; <>"), "<secret:Q> <>");
    }

    /// Property, with random values (a fixed sequence): values made of the
    /// characters encodings change, encoded by random chains of up to four
    /// of [`encoders`], are masked; and random texts of escapes without the
    /// value are left byte for byte.
    #[test]
    fn random_values_and_chains_are_masked() {
        let alphabet: Vec<char> = "ab9 \"'\\&<>%+/=?#;:é€😀\t-_.~".chars().collect();
        let encoders = encoders();
        let mut random = Xorshift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..400 {
            let length = 4 + random.below(24);
            let mut value: String = (0..length)
                .map(|_| alphabet[random.below(alphabet.len())])
                .collect();
            value.push_str("Zq");
            let value = value.trim().replace(['"', '\''], "x");
            let s = Secrets::parse(&format!("PW='{value}'\n")).unwrap();
            let chain: Vec<usize> = (0..1 + random.below(4))
                .map(|_| random.below(encoders.len()))
                .collect();
            let encoded = chain
                .iter()
                .fold(value.clone(), |text, &e| (encoders[e].1)(&text));
            let masked = s.mask(&format!("<p>{encoded}</p>"));
            let names: Vec<&str> = chain.iter().map(|&e| encoders[e].0).collect();
            assert!(
                masked.contains("<secret:PW>") && !masked.contains(&encoded),
                "{value:?} {names:?}: {masked}"
            );
            // And a text of escapes without the value is left as it is
            let noise: String = (0..64)
                .map(|_| {
                    [
                        "\\\\", "\\\"", "&amp;", "&#39;", "%25", "%2B", "+", " ", "\\u0026", "a",
                        "é",
                    ][random.below(11)]
                })
                .collect();
            assert_eq!(s.mask(&noise), noise, "{value:?}");
        }
    }

    /// A value whose last characters start an escape the text around it
    /// completes (a trailing `\` before an attribute's closing `"`, a `%`
    /// before hex digits, an `&` before `amp;`), or whose first character
    /// completes one the text around it starts (a `\` before an `n`), is
    /// masked whole: the escape it shares with its surroundings is masked
    /// with it.
    #[test]
    fn values_sharing_an_escape_with_their_surroundings_are_masked() {
        let s = Secrets::parse(
            "B='Qx9\"w&<z>7731\\'\nP='Qx9&w7731%'\nA='Qx9<w7731&'\nN='nQx9&w7731'\n",
        )
        .unwrap();
        for text in [
            // Raw HTML: an attribute, its JSON, a text node (inspect_dom)
            r#"<div a="Qx9&quot;w&amp;&lt;z&gt;7731\" b=1>"#,
            r#"<div j="{&quot;p&quot;:&quot;Qx9\&quot;w&amp;&lt;z&gt;7731\\&quot;}">"#,
            r"<p>Qx9&quot;w&amp;&lt;z&gt;7731\n</p>",
            r"'Qx9&quot;w&amp;&lt;z&gt;7731\'",
            r"Qx9&quot;w&amp;&lt;z&gt;7731\u0041",
            "Qx9&amp;w7731%41",
            "Qx9&lt;w7731&amp;amp;",
            r"C:\nQx9&amp;w7731",
        ] {
            let masked = s.mask(text);
            assert!(
                !masked.contains("7731") && masked.contains("<secret:"),
                "{text}: {masked}"
            );
        }
        // And the text around a value that shares no escape stays whole
        assert_eq!(s.mask(r#"<i a="Qx9&amp;w7731%">"#), r#"<i a="<secret:P>">"#);
    }

    /// Only a whole value is masked: what a tool's size limit, a window the
    /// caller chose or the page's own code left of one is text like any
    /// other (the agent can ask for any part of a value with `slice()`;
    /// guessing parts next to a cut mark missed cuts without a mark and
    /// cuts inside an escape, #181-#183). The whole value beside a mark is
    /// still masked.
    #[test]
    fn only_whole_values_are_masked() {
        let s = Secrets::parse("PW='Qx9\"w&<z> Zk7731'\n").unwrap();
        for text in [
            "<p>Qx9\"w&amp;&lt;z&gt; Zk7\n[...]",
            "[...]\nw&amp;&lt;z&gt; Zk7731</p>",
            "line Qx9\"w&<z> Zk… [cut: the first 9 of 99 characters]",
            "Qx9\"w&<z> Zk773",
            "x9\"w&<z> Zk7731",
        ] {
            assert_eq!(s.mask(text), text);
        }
        assert_eq!(
            s.mask("Qx9\"w&amp;&lt;z&gt; Zk7731… [cut]\n[...]"),
            "<secret:PW>… [cut]\n[...]"
        );
    }

    /// #184: a reply made of cut marks after an escape encoded six times
    /// (every layer is decoded) is masked in a time that does not grow
    /// with the marks times the secrets: well under the 3 s of #145, with
    /// 100 secrets.
    #[test]
    fn a_reply_full_of_cut_marks_is_masked_quickly() {
        let file: String = (0..100)
            .map(|i| format!("S{i:03}='Pw{i:03}&<\"x y'\n"))
            .collect();
        let s = Secrets::parse(&file).unwrap();
        let reply = format!("%25252525252541{} Pw050&<\"x y", "\u{2026}".repeat(99_985));
        let started = std::time::Instant::now();
        let masked = s.mask(&reply);
        let elapsed = started.elapsed();
        assert!(masked.ends_with(" <secret:S050>"));
        assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
    }

    /// #191: a reply where every position starts ten overlapping secrets,
    /// after one escape encoded six times (every layer is decoded), is
    /// masked in a time that does not grow with the matches times the
    /// layers: a layer only adds what its decoded escapes changed, and the
    /// matches are merged without sorting them. Well under the 3 s of the
    /// README, with 100 secrets.
    #[test]
    fn overlapping_secrets_in_every_layer_are_masked_quickly() {
        let period = "Qx7Lm9Pz2R".repeat(3);
        let file: String = (0..10)
            .flat_map(|o| (4..14).map(move |l| (o, l)))
            .enumerate()
            .map(|(i, (o, l))| format!("S{i:03}='{}'\n", &period[o..o + l]))
            .collect();
        let s = Secrets::parse(&file).unwrap();
        let reply = format!("%25252525252541{}", "Qx7Lm9Pz2R".repeat(9990));
        let started = std::time::Instant::now();
        let masked = s.mask(&reply);
        let elapsed = started.elapsed();
        assert!(
            masked.starts_with("%25252525252541<secret:S"),
            "{}",
            &masked[..60]
        );
        assert_eq!(masked.matches("<secret:").count(), 1);
        assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
    }

    /// A small deterministic random sequence (xorshift64).
    struct Xorshift(u64);

    impl Xorshift {
        /// A number below `n`.
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(n).unwrap()).unwrap()
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
