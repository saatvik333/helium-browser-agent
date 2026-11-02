//! US keyboard layout (vendored from Playwright) and key-chord parsing.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use serde::Deserialize;

const LAYOUT: &str = include_str!("../assets/playwright/us-keyboard-layout.json");

/// Location value of numeric-keypad keys.
const KEYPAD: u8 = 3;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    key_code: u32,
    #[serde(default)]
    shift_key_code: Option<u32>,
    key: String,
    #[serde(default)]
    shift_key: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    location: u8,
}

/// Everything `Input.dispatchKeyEvent` needs for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(clippy::struct_field_names, reason = "fields mirror KeyboardEvent's key and keyCode")]
pub struct Key {
    pub key: String,
    pub code: String,
    pub key_code: u32,
    pub text: String,
    pub location: u8,
    shifted: Option<Box<Key>>,
}

impl Key {
    pub fn is_keypad(&self) -> bool {
        self.location == KEYPAD
    }
}

pub const MODIFIERS: [&str; 4] = ["Alt", "Control", "Meta", "Shift"];

/// CDP modifier bit for a modifier key name.
pub fn modifier_bit(name: &str) -> i64 {
    match name {
        "Alt" => 1,
        "Control" => 2,
        "Meta" => 4,
        "Shift" => 8,
        _ => 0,
    }
}

static KEYS: LazyLock<HashMap<String, Key>> = LazyLock::new(build);

/// Playwright's `buildLayoutClosure`: keys by code, by key text and by alias.
fn build() -> HashMap<String, Key> {
    let definitions: HashMap<String, Definition> =
        serde_json::from_str(LAYOUT).expect("vendored keyboard layout is valid JSON");
    let mut keys = HashMap::new();
    for (code, definition) in definitions {
        let text = match &definition.text {
            Some(text) => text.clone(),
            None if definition.key.chars().count() == 1 => definition.key.clone(),
            None => String::new(),
        };
        let base = Key {
            key: definition.key.clone(),
            code: code.clone(),
            key_code: definition.key_code,
            text: if definition.key.chars().count() == 1 { definition.key.clone() } else { text },
            location: definition.location,
            shifted: None,
        };
        let shifted = definition.shift_key.as_ref().map(|shift_key| Key {
            key: shift_key.clone(),
            text: shift_key.clone(),
            key_code: definition.shift_key_code.unwrap_or(definition.key_code),
            ..base.clone()
        });
        keys.insert(code.clone(), Key { shifted: shifted.clone().map(Box::new), ..base.clone() });
        let aliases: &[&str] = match code.as_str() {
            "ShiftLeft" => &["Shift"],
            "ControlLeft" => &["Control"],
            "AltLeft" => &["Alt"],
            "MetaLeft" => &["Meta"],
            "Enter" => &["\n", "\r"],
            _ => &[],
        };
        for alias in aliases {
            keys.insert((*alias).to_owned(), base.clone());
        }
        if definition.location != 0 {
            continue;
        }
        if base.key.chars().count() == 1 {
            keys.insert(base.key.clone(), base.clone());
        }
        if let Some(shifted) = shifted {
            keys.insert(shifted.key.clone(), shifted);
        }
    }
    keys
}

/// Looks up a key by `code` (`KeyA`), `key` (`a`, `A`, `Enter`) or alias, as Playwright does.
pub fn key(name: &str, shift_held: bool) -> Result<Key> {
    let name = if name == "ControlOrMeta" { "Control" } else { name };
    let Some(found) = KEYS.get(name) else {
        bail!("unknown key {name:?}; use names like Enter, Tab, ArrowDown, KeyA, a, F5");
    };
    Ok(match (&found.shifted, shift_held) {
        (Some(shifted), true) => (**shifted).clone(),
        _ => found.clone(),
    })
}

/// True when `c` can be typed with a key of the layout (otherwise it is inserted as text).
pub fn typeable(c: char) -> bool {
    let mut buffer = [0; 4];
    KEYS.contains_key(c.encode_utf8(&mut buffer) as &str)
}

/// Splits `Control+Shift+a` into keys; a trailing `+` is the plus key itself.
pub fn split_chord(chord: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut current = String::new();
    for c in chord.chars() {
        if c == '+' && !current.is_empty() {
            keys.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    keys.push(current);
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_resolve_by_code_key_and_alias() {
        assert_eq!(key("KeyA", false).unwrap().key, "a");
        assert_eq!(key("KeyA", true).unwrap().key, "A");
        assert_eq!(key("A", false).unwrap().code, "KeyA");
        assert_eq!(key("Enter", false).unwrap().text, "\r");
        assert_eq!(key("\n", false).unwrap().code, "Enter");
        assert_eq!(key("Shift", false).unwrap().code, "ShiftLeft");
        assert_eq!(key("ControlOrMeta", false).unwrap().key, "Control");
        assert_eq!(key("!", false).unwrap().code, "Digit1");
        assert!(key("Banana", false).is_err());
    }

    #[test]
    fn keypad_keys_are_marked() {
        assert!(key("Numpad0", false).unwrap().is_keypad());
        assert!(!key("Digit0", false).unwrap().is_keypad());
    }

    #[test]
    fn chords_split_like_playwright() {
        assert_eq!(split_chord("Control+Shift+K"), ["Control", "Shift", "K"]);
        assert_eq!(split_chord("Shift++"), ["Shift", "+"]);
        assert_eq!(split_chord("+"), ["+"]);
    }

    #[test]
    fn typeable_covers_ascii_but_not_emoji() {
        assert!(typeable('a') && typeable('~') && typeable('\n'));
        assert!(!typeable('é') && !typeable('🙂'));
    }
}
