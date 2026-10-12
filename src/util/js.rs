//! JavaScript snippets the tools evaluate in the page, shared by every
//! backend. Values from the agent reach JavaScript only as JSON string
//! literals, never by hand-escaping (a `\` used to break out of the string).

/// `value` as a JavaScript string literal (JSON strings are valid JS).
pub fn string_literal(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// `(function() { const el = <selector>; if (!el) return <missing>; <body> })()`:
/// the one shape every element snippet has.
fn on_element(selector: &str, missing: &str, body: &str) -> String {
    format!(
        "(function() {{ const el = document.querySelector({}); if (!el) return {missing}; {body} }})()",
        string_literal(selector)
    )
}

/// Scrolls the first element matching `selector` into view; `true` if found.
pub fn scroll_into_view(selector: &str) -> String {
    on_element(
        selector,
        "false",
        "el.scrollIntoView({ behavior: 'instant', block: 'center' }); return true;",
    )
}

/// The viewport centre `{x, y}` of the first element matching `selector`, or `null`.
pub fn element_center(selector: &str) -> String {
    on_element(
        selector,
        "null",
        "const rect = el.getBoundingClientRect(); \
         return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };",
    )
}

/// Focuses the first element matching `selector` and empties it, so the
/// text typed next replaces its content; `true` if found.
pub fn focus_and_clear(selector: &str) -> String {
    on_element(
        selector,
        "false",
        "el.focus(); \
         if ('value' in el) { el.value = ''; } else if (el.isContentEditable) { el.textContent = ''; } \
         el.dispatchEvent(new Event('input', { bubbles: true })); return true;",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument of `document.querySelector(..)` in `expression`, parsed back as JSON.
    fn embedded_selector(expression: &str) -> String {
        let start = expression.find("querySelector(").unwrap() + "querySelector(".len();
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
            element_center(selector),
            focus_and_clear(selector),
        ] {
            // Then it is one JSON string literal that parses back to the original
            assert_eq!(embedded_selector(&expression), selector, "{expression}");
        }
    }

    /// FIX-07: fill_input replaces the existing value.
    #[test]
    fn fill_input_replaces_value() {
        let expression = focus_and_clear("#q");
        assert!(expression.contains("el.focus()"));
        assert!(expression.contains("el.value = ''"), "{expression}");
    }
}
