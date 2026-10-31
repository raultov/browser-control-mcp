//! `--secrets <file>`: values the agent should not read back. Every tool
//! result and error has them replaced by `<secret:NAME>`, and `fill_input`
//! and `fill_form` type the value of a `<secret:NAME>` they are given, so a
//! password can be typed without the agent ever seeing it.
//!
//! A convenience, not a security boundary: a page can transform a value
//! (encode it, split it) into something no longer masked, and images
//! (screenshots, PDFs) are not masked.

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
        Ok(Self { entries })
    }

    /// Whether no secret is configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `text` with every secret value (as is, and as escaped inside a JSON
    /// string) replaced by `<secret:NAME>`.
    pub fn mask(&self, text: &str) -> String {
        let forms = self.forms();
        if !forms.iter().any(|(form, _)| text.contains(form.as_str())) {
            return text.to_string();
        }
        // One pass over the original text, so a token already put in is
        // never matched again.
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'scan: while let Some(c) = rest.chars().next() {
            for (form, token) in &forms {
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

    /// Each value, as is and as escaped inside a JSON string, with its
    /// token: the longest first, so a value containing another is masked whole.
    fn forms(&self) -> Vec<(String, String)> {
        let mut forms = Vec::new();
        for (name, value) in &self.entries {
            let token = format!("<secret:{name}>");
            let escaped = serde_json::to_string(value).unwrap_or_default();
            let escaped = escaped.trim_matches('"');
            if escaped != value {
                forms.push((escaped.to_string(), token.clone()));
            }
            forms.push((value.clone(), token));
        }
        forms.sort_by_key(|(form, _)| std::cmp::Reverse(form.len()));
        forms
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
