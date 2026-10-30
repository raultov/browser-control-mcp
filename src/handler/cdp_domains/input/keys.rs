//! Keys, modifiers and mouse buttons of the input tools, with what each
//! protocol needs: Chrome's `Input.dispatchKeyEvent` fields and modifier
//! bits, and WebDriver's key code points (BiDi `input.performActions`).

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use crate::handler::interruption::Interruption;
use rust_mcp_sdk::macros;
use rust_mcp_sdk::schema::CallToolError;
use serde_json::{Value, json};

/// A modifier key held during a key press or a click.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum Modifier {
    /// Alt (Option on macOS).
    #[serde(rename = "Alt")]
    Alt,
    /// Control.
    #[serde(rename = "Control")]
    Control,
    /// Meta (Command on macOS, Windows key elsewhere).
    #[serde(rename = "Meta")]
    Meta,
    /// Shift.
    #[serde(rename = "Shift")]
    Shift,
}

impl Modifier {
    /// Its bit in CDP's `modifiers`.
    fn cdp_bit(self) -> u8 {
        match self {
            Self::Alt => 1,
            Self::Control => 2,
            Self::Meta => 4,
            Self::Shift => 8,
        }
    }

    /// WebDriver's code point for it.
    fn webdriver(self) -> char {
        match self {
            Self::Alt => '\u{E00A}',
            Self::Control => '\u{E009}',
            Self::Meta => '\u{E03D}',
            Self::Shift => '\u{E008}',
        }
    }

    /// Its Windows key code.
    fn key_code(self) -> u32 {
        match self {
            Self::Alt => 18,
            Self::Control => 17,
            Self::Meta => 91,
            Self::Shift => 16,
        }
    }

    /// CDP key event `kind` of this key, with `modifiers` held.
    fn cdp_event(self, kind: &str, modifiers: u8) -> Value {
        let name = self.name();
        json!({
            "type": kind, "key": name, "code": format!("{name}Left"), "modifiers": modifiers,
            "windowsVirtualKeyCode": self.key_code(), "nativeVirtualKeyCode": self.key_code(),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::Control => "Control",
            Self::Meta => "Meta",
            Self::Shift => "Shift",
        }
    }
}

/// CDP's `modifiers` bit mask for `held`.
pub(crate) fn cdp_modifiers(held: &[Modifier]) -> u8 {
    held.iter().fold(0, |bits, m| bits | m.cdp_bit())
}

/// CDP key events pressing `held` (in order, each with the bits held so
/// far) and releasing them (in reverse), as a user holding them: the page
/// sees their keydown and keyup, as with Firefox's key actions.
pub(crate) fn cdp_modifier_events(held: &[Modifier]) -> (Vec<Value>, Vec<Value>) {
    let mut bits = 0;
    let mut downs = Vec::new();
    for m in held {
        bits |= m.cdp_bit();
        downs.push(m.cdp_event("rawKeyDown", bits));
    }
    let mut ups = Vec::new();
    for m in held.iter().rev() {
        bits &= !m.cdp_bit();
        ups.push(m.cdp_event("keyUp", bits));
    }
    (downs, ups)
}

/// Runs `action` (key or mouse events carrying the modifier bits) with
/// `held` pressed before and released after, on Chrome. When the action
/// failed or was interrupted the releases go in the background, so no
/// modifier is left held.
pub(crate) async fn holding(
    target: &CdpTarget,
    held: &[Modifier],
    action: impl Future<Output = Result<Option<Interruption>, CallToolError>>,
) -> Result<Option<Interruption>, CallToolError> {
    let method = "Input.dispatchKeyEvent";
    let (downs, ups) = cdp_modifier_events(held);
    for down in downs {
        crate::handler::send_or(target, method, down, "CDP Error").await?;
    }
    let release_later = |target: CdpTarget, ups: Vec<Value>| {
        tokio::spawn(async move {
            for up in ups {
                let _ = target.send_raw_command(method, up).await;
            }
        });
    };
    let interrupted = match action.await {
        Ok(interrupted) => interrupted,
        Err(error) => {
            release_later(target.clone(), ups);
            return Err(error);
        }
    };
    if interrupted.is_some() {
        release_later(target.clone(), ups);
        return Ok(interrupted);
    }
    for up in ups {
        crate::handler::send_or(target, method, up, "CDP Error").await?;
    }
    Ok(None)
}

/// `Control+Shift+` for `held` (empty when none), for replies.
pub(crate) fn chord(held: &[Modifier]) -> String {
    held.iter().map(|m| format!("{}+", m.name())).collect()
}

/// BiDi key actions pressing `held` (in order), then `inner` pauses (while
/// another source acts), then releasing them (in reverse order).
pub(crate) fn held_keys(held: &[Modifier], inner: usize) -> Vec<Value> {
    let key = |kind: &str, m: &Modifier| json!({"type": kind, "value": m.webdriver().to_string()});
    let down = held.iter().map(|m| key("keyDown", m));
    let pauses = std::iter::repeat_n(json!({"type": "pause"}), inner);
    let up = held.iter().rev().map(|m| key("keyUp", m));
    down.chain(pauses).chain(up).collect()
}

/// A mouse button of `click_element`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    ::serde::Deserialize,
    ::serde::Serialize,
    macros::JsonSchema,
)]
pub enum MouseButton {
    /// The main button.
    #[default]
    #[serde(rename = "left")]
    Left,
    /// The secondary button (opens context menus).
    #[serde(rename = "right")]
    Right,
    /// The wheel button.
    #[serde(rename = "middle")]
    Middle,
}

impl MouseButton {
    /// CDP's `button`.
    pub(crate) fn cdp(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    /// WebDriver's button number.
    pub(crate) fn webdriver(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// A key `press_key` can press.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Key {
    /// DOM `key` (`Enter`, `a`).
    pub(crate) key: String,
    /// DOM `code` (`Enter`, `KeyA`; empty when it has none).
    pub(crate) code: String,
    /// Windows virtual key code (CDP).
    pub(crate) key_code: u32,
    /// The text it types, when it types one.
    pub(crate) text: Option<String>,
    /// WebDriver's code point (the character itself for a printable key).
    pub(crate) webdriver: char,
}

/// A named key: its name (and DOM `code`), Windows key code, text and
/// WebDriver code point.
struct Named {
    name: &'static str,
    key_code: u32,
    text: Option<&'static str>,
    webdriver: char,
}

const NAMED: &[Named] = &[
    Named {
        name: "Enter",
        key_code: 13,
        text: Some("\r"),
        webdriver: '\u{E007}',
    },
    Named {
        name: "Tab",
        key_code: 9,
        text: None,
        webdriver: '\u{E004}',
    },
    Named {
        name: "Escape",
        key_code: 27,
        text: None,
        webdriver: '\u{E00C}',
    },
    Named {
        name: "Backspace",
        key_code: 8,
        text: None,
        webdriver: '\u{E003}',
    },
    Named {
        name: "Delete",
        key_code: 46,
        text: None,
        webdriver: '\u{E017}',
    },
    Named {
        name: "ArrowLeft",
        key_code: 37,
        text: None,
        webdriver: '\u{E012}',
    },
    Named {
        name: "ArrowUp",
        key_code: 38,
        text: None,
        webdriver: '\u{E013}',
    },
    Named {
        name: "ArrowRight",
        key_code: 39,
        text: None,
        webdriver: '\u{E014}',
    },
    Named {
        name: "ArrowDown",
        key_code: 40,
        text: None,
        webdriver: '\u{E015}',
    },
    Named {
        name: "Home",
        key_code: 36,
        text: None,
        webdriver: '\u{E011}',
    },
    Named {
        name: "End",
        key_code: 35,
        text: None,
        webdriver: '\u{E010}',
    },
    Named {
        name: "PageUp",
        key_code: 33,
        text: None,
        webdriver: '\u{E00E}',
    },
    Named {
        name: "PageDown",
        key_code: 34,
        text: None,
        webdriver: '\u{E00F}',
    },
    Named {
        name: "Insert",
        key_code: 45,
        text: None,
        webdriver: '\u{E016}',
    },
    Named {
        name: "Space",
        key_code: 32,
        text: Some(" "),
        webdriver: '\u{E00D}',
    },
];

/// The key `name` names: a named key (case-insensitive), `F1`..`F12`, or
/// one character.
pub(crate) fn key(name: &str) -> Result<Key, String> {
    if let Some(named) = NAMED.iter().find(|n| n.name.eq_ignore_ascii_case(name)) {
        let key = if named.name == "Space" {
            " "
        } else {
            named.name
        };
        return Ok(Key {
            key: key.into(),
            code: named.name.into(),
            key_code: named.key_code,
            text: named.text.map(str::to_string),
            webdriver: named.webdriver,
        });
    }
    if let Some(n) = function_key(name) {
        return Ok(Key {
            key: format!("F{n}"),
            code: format!("F{n}"),
            key_code: 111 + n,
            text: None,
            webdriver: char::from_u32(0xE031 + n - 1).unwrap_or('\u{E031}'),
        });
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(printable(c)),
        _ => {
            let names: Vec<&str> = NAMED.iter().map(|n| n.name).collect();
            Err(format!(
                "Unknown key '{name}': give one character, F1 to F12, or one of {}",
                names.join(", ")
            ))
        }
    }
}

/// `F1`..`F12`'s number.
fn function_key(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('F').or_else(|| name.strip_prefix('f'))?;
    rest.parse().ok().filter(|n| (1..=12).contains(n))
}

/// A one-character key: it types itself.
fn printable(c: char) -> Key {
    let upper = c.to_ascii_uppercase();
    let (code, key_code) = if c.is_ascii_alphabetic() {
        (format!("Key{upper}"), u32::from(upper))
    } else if c.is_ascii_digit() {
        (format!("Digit{c}"), u32::from(c))
    } else {
        (String::new(), 0)
    };
    Key {
        key: c.to_string(),
        code,
        key_code,
        text: Some(c.to_string()),
        webdriver: c,
    }
}

impl Key {
    /// The CDP `Input.dispatchKeyEvent` params of its press and release,
    /// with `held` down. A key types its text only without Control, Meta or
    /// Alt (a shortcut such as Control+A types nothing).
    pub(crate) fn cdp_events(&self, held: &[Modifier]) -> [Value; 2] {
        let modifiers = cdp_modifiers(held);
        let shortcut = held.iter().any(|m| *m != Modifier::Shift);
        let text = self.text.as_ref().filter(|_| !shortcut);
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": self.key, "code": self.code, "modifiers": modifiers,
            "windowsVirtualKeyCode": self.key_code, "nativeVirtualKeyCode": self.key_code,
        });
        if let Some(text) = text {
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        let up = json!({
            "type": "keyUp", "key": self.key, "code": self.code, "modifiers": modifiers,
            "windowsVirtualKeyCode": self.key_code, "nativeVirtualKeyCode": self.key_code,
        });
        [down, up]
    }

    /// The BiDi key actions of its press with `held` down.
    pub(crate) fn bidi_actions(&self, held: &[Modifier]) -> Value {
        let mut actions = held_keys(held, 2);
        let value = self.webdriver.to_string();
        let at = held.len();
        actions[at] = json!({"type": "keyDown", "value": value});
        actions[at + 1] = json!({"type": "keyUp", "value": value});
        json!([{"type": "key", "id": "keyboard", "actions": actions}])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-7: named keys, function keys and characters map to both
    /// protocols; anything else is refused listing the names.
    #[test]
    fn keys_map_to_both_protocols() {
        let enter = key("enter").unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.key_code, enter.webdriver),
            ("Enter", 13, '\u{E007}')
        );
        assert_eq!(enter.text.as_deref(), Some("\r"));
        let space = key("Space").unwrap();
        assert_eq!((space.key.as_str(), space.code.as_str()), (" ", "Space"));
        let f5 = key("F5").unwrap();
        assert_eq!((f5.key_code, f5.webdriver), (116, '\u{E035}'));
        let a = key("a").unwrap();
        assert_eq!(
            (a.code.as_str(), a.key_code, a.webdriver),
            ("KeyA", 65, 'a')
        );
        let error = key("Hyper").unwrap_err();
        assert!(
            error.contains("Unknown key 'Hyper'") && error.contains("Escape"),
            "{error}"
        );
        assert!(key("F13").is_err());
    }

    /// ISSUE-7: a shortcut types nothing on Chrome; Shift alone still types.
    #[test]
    fn shortcuts_type_nothing() {
        let a = key("a").unwrap();
        let [down, up] = a.cdp_events(&[Modifier::Control]);
        assert_eq!(down["type"], "rawKeyDown");
        assert!(down.get("text").is_none());
        assert_eq!(
            (down["modifiers"].clone(), up["type"].clone()),
            (json!(2), json!("keyUp"))
        );
        let [shifted, _] = a.cdp_events(&[Modifier::Shift]);
        assert_eq!(
            (shifted["type"].clone(), shifted["text"].clone()),
            (json!("keyDown"), json!("a"))
        );
    }

    /// ISSUE-7: on Firefox the modifiers wrap the key, released in reverse.
    #[test]
    fn bidi_modifiers_wrap_the_key() {
        let held = [Modifier::Control, Modifier::Shift];
        let actions = key("Tab").unwrap().bidi_actions(&held);
        let values: Vec<String> = actions[0]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                format!(
                    "{} {}",
                    a["type"].as_str().unwrap(),
                    a["value"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                "keyDown \u{E009}",
                "keyDown \u{E008}",
                "keyDown \u{E004}",
                "keyUp \u{E004}",
                "keyUp \u{E008}",
                "keyUp \u{E009}"
            ]
        );
        assert_eq!(chord(&held), "Control+Shift+");
        assert_eq!(cdp_modifiers(&held), 10);
    }
}
