//! Direct text typing: per-text XKB keymaps that map each character to a
//! key, so no application-specific paste chord is needed.
//!
//! Characters are placed only on keys that are printable on a real
//! keyboard (digits/letters/punctuation rows). Some clients (Chromium)
//! interpret physical control positions such as evdev 14 (Backspace)
//! despite a printable keysym, see wtype#77. With only 48 such keys, longer
//! texts are split into batches, each with its own keymap.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Deserialize;
use xkbcommon::xkb;

use super::virtual_keyboard::{Batch, Stroke};

/// How a newline in the text is typed.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Newline {
    /// Shift+Return: a line break without sending in most chat apps.
    #[default]
    ShiftEnter,
    /// Plain Return (may submit forms/messages or run shell commands).
    Enter,
    /// Replace line breaks with a space.
    Space,
}

/// evdev codes of keys that are printable on a US keyboard.
const PRINTABLE_KEYS: [u32; 48] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, // 1 … =
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, // q … ]
    30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, // a … `
    43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, // \ z … /
    86, // 102nd key
];
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_LEFTSHIFT: u32 = 42;
/// XKB keycodes are evdev codes + 8.
const XKB_OFFSET: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum TypingError {
    #[error("could not compile the typing keymap")]
    Keymap,
    #[error("the typing keymap has no Shift modifier")]
    NoShift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(xkb::Keysym),
    Return { shift: bool },
    Tab,
}

/// Turn `text` into keymap batches and key strokes.
pub fn plan(text: &str, newline: Newline) -> Result<Vec<Batch>, TypingError> {
    let keys = keys(text, newline);
    let mut batches = Vec::new();
    let mut start = 0;
    while start < keys.len() {
        // Take as many keys as fit into one keymap's printable slots.
        let mut slots: HashMap<xkb::Keysym, u32> = HashMap::new();
        let mut end = start;
        while end < keys.len() {
            if let Key::Char(sym) = keys[end]
                && !slots.contains_key(&sym)
            {
                if slots.len() == PRINTABLE_KEYS.len() {
                    break;
                }
                slots.insert(sym, PRINTABLE_KEYS[slots.len()]);
            }
            end += 1;
        }
        batches.push(batch(&keys[start..end], &slots)?);
        start = end;
    }
    Ok(batches)
}

fn keys(text: &str, newline: Newline) -> Vec<Key> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut keys = Vec::with_capacity(text.len());
    for c in text.chars() {
        let key = match c {
            '\n' => match newline {
                Newline::ShiftEnter => Key::Return { shift: true },
                Newline::Enter => Key::Return { shift: false },
                Newline::Space => Key::Char(xkb::Keysym::space),
            },
            '\t' => Key::Tab,
            c if c.is_control() => continue,
            c => {
                let sym = xkb::utf32_to_keysym(c as u32);
                if sym == xkb::Keysym::NoSymbol {
                    continue;
                }
                Key::Char(sym)
            }
        };
        keys.push(key);
    }
    keys
}

fn batch(keys: &[Key], slots: &HashMap<xkb::Keysym, u32>) -> Result<Batch, TypingError> {
    let mut codes = String::new();
    let mut symbols = String::new();
    let mut fixed: Vec<(u32, String)> = vec![
        (KEY_TAB, "Tab".into()),
        (KEY_ENTER, "Return".into()),
        (KEY_LEFTSHIFT, "Shift_L".into()),
    ];
    fixed.extend(
        slots
            .iter()
            .map(|(sym, code)| (*code, xkb::keysym_get_name(*sym))),
    );
    fixed.sort_by_key(|(code, _)| *code);
    for (code, name) in &fixed {
        let xkb_code = code + XKB_OFFSET;
        let _ = writeln!(codes, "    <I{xkb_code}> = {xkb_code};");
        let _ = writeln!(symbols, "    key <I{xkb_code}> {{ [ {name} ] }};");
    }
    let shift_code = KEY_LEFTSHIFT + XKB_OFFSET;
    let source = format!(
        "xkb_keymap {{\n\
         xkb_keycodes \"wispr\" {{\n    minimum = 8;\n    maximum = 255;\n{codes}}};\n\
         xkb_types \"wispr\" {{ include \"complete\" }};\n\
         xkb_compatibility \"wispr\" {{ include \"complete\" }};\n\
         xkb_symbols \"wispr\" {{\n{symbols}    modifier_map Shift {{ <I{shift_code}> }};\n}};\n\
         }};\n"
    );

    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &ctx,
        source,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or(TypingError::Keymap)?;
    let shift = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    if shift == xkb::MOD_INVALID {
        return Err(TypingError::NoShift);
    }
    let shift = 1 << shift;

    let strokes = keys
        .iter()
        .map(|key| match key {
            Key::Char(sym) => Stroke {
                key: slots[sym],
                mods: 0,
            },
            Key::Return { shift: s } => Stroke {
                key: KEY_ENTER,
                mods: if *s { shift } else { 0 },
            },
            Key::Tab => Stroke {
                key: KEY_TAB,
                mods: 0,
            },
        })
        .collect();

    let mut text = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    text.push(0);
    Ok(Batch {
        keymap: text,
        strokes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve strokes back to text with the batch's own keymap.
    fn typed(batches: &[Batch]) -> String {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let mut out = String::new();
        for b in batches {
            let source = String::from_utf8(b.keymap[..b.keymap.len() - 1].to_vec()).unwrap();
            let keymap = xkb::Keymap::new_from_string(
                &ctx,
                source,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let mut state = xkb::State::new(&keymap);
            for s in &b.strokes {
                state.update_mask(s.mods, 0, 0, 0, 0, 0);
                let code = xkb::Keycode::new(s.key + XKB_OFFSET);
                match state.key_get_one_sym(code) {
                    xkb::Keysym::Return if s.mods != 0 => out.push_str("<S-\n>"),
                    xkb::Keysym::Return => out.push('\n'),
                    xkb::Keysym::Tab => out.push('\t'),
                    sym => out.push(char::from_u32(xkb::keysym_to_utf32(sym)).unwrap()),
                }
            }
        }
        out
    }

    #[test]
    fn types_unicode_text() {
        let text = "Grüße, 世界 👋 a+b=c?";
        let batches = plan(text, Newline::Enter).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(typed(&batches), text);
        assert_eq!(batches[0].keymap.last(), Some(&0));
    }

    #[test]
    fn uses_only_printable_positions() {
        let batches = plan("Hello. World!", Newline::Enter).unwrap();
        for s in &batches[0].strokes {
            assert!(PRINTABLE_KEYS.contains(&s.key), "key {}", s.key);
        }
    }

    #[test]
    fn handles_newlines_tabs_and_control_chars() {
        let text = "a\r\nb\tc\u{7}d\re";
        assert_eq!(
            typed(&plan(text, Newline::ShiftEnter).unwrap()),
            "a<S-\n>b\tcd<S-\n>e"
        );
        assert_eq!(typed(&plan(text, Newline::Enter).unwrap()), "a\nb\tcd\ne");
        assert_eq!(typed(&plan(text, Newline::Space).unwrap()), "a b\tcd e");
    }

    #[test]
    fn splits_many_distinct_chars_into_batches() {
        let text: String = (0..200u32)
            .map(|i| char::from_u32(0x400 + i).unwrap())
            .collect::<String>()
            + "\nend";
        let batches = plan(&text, Newline::Enter).unwrap();
        assert!(batches.len() >= 5);
        assert_eq!(typed(&batches), text);
    }

    #[test]
    fn empty_text_has_no_batches() {
        assert!(plan("", Newline::Enter).unwrap().is_empty());
        assert!(plan("\u{1}", Newline::Enter).unwrap().is_empty());
    }
}
