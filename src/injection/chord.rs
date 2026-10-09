//! Paste chord parsing and the XKB keymap uploaded to the virtual keyboard.

use std::fmt;
use std::str::FromStr;

use xkbcommon::xkb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    fn xkb_name(self) -> &'static str {
        match self {
            Modifier::Ctrl => xkb::MOD_NAME_CTRL,
            Modifier::Shift => xkb::MOD_NAME_SHIFT,
            Modifier::Alt => xkb::MOD_NAME_ALT,
            Modifier::Super => xkb::MOD_NAME_LOGO,
        }
    }
}

/// A key combination such as `ctrl+v`, `ctrl+shift+v`, or `shift+Insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<Modifier>,
    pub keysym: xkb::Keysym,
    name: String,
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChordError {
    #[error("empty key chord")]
    Empty,
    #[error("unknown modifier `{0}` (use ctrl, shift, alt, or super)")]
    UnknownModifier(String),
    #[error("duplicate modifier `{0}`")]
    DuplicateModifier(String),
    #[error("unknown key `{0}` (use an XKB keysym name such as v or Insert)")]
    UnknownKey(String),
}

impl FromStr for Chord {
    type Err = ChordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key, mods) = parts.split_last().ok_or(ChordError::Empty)?;
        if key.is_empty() {
            return Err(ChordError::Empty);
        }

        let mut modifiers = Vec::new();
        for m in mods {
            let modifier = match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Modifier::Ctrl,
                "shift" => Modifier::Shift,
                "alt" => Modifier::Alt,
                "super" | "logo" | "mod4" => Modifier::Super,
                _ => return Err(ChordError::UnknownModifier((*m).to_owned())),
            };
            if modifiers.contains(&modifier) {
                return Err(ChordError::DuplicateModifier((*m).to_owned()));
            }
            modifiers.push(modifier);
        }

        // Case-insensitive lookup prefers lowercase, so `V` resolves to `v`;
        // the shift level must be requested explicitly with `shift+`.
        let keysym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
        if keysym == xkb::Keysym::NoSymbol {
            return Err(ChordError::UnknownKey((*key).to_owned()));
        }

        Ok(Self {
            modifiers,
            keysym,
            name: s.trim().to_owned(),
        })
    }
}

/// Everything needed to emit a chord through a virtual keyboard.
#[derive(Debug, Clone)]
pub struct ChordPlan {
    /// NUL-terminated XKB text keymap.
    pub keymap: Vec<u8>,
    /// Wire keycode (evdev = XKB keycode - 8).
    pub key: u32,
    /// Depressed-modifier mask for the chord's modifiers in `keymap`.
    pub mods: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("could not compile the XKB keymap (is xkeyboard-config installed?)")]
    Keymap,
    #[error("key `{0}` is not on the base level of the US keymap")]
    KeyNotFound(String),
    #[error("modifier {0} is missing from the keymap")]
    ModifierNotFound(&'static str),
}

/// Build a self-contained US keymap and resolve the chord against it.
///
/// The keymap is fixed (not the user's layout) so keycodes and modifier
/// indices are deterministic; the focused client receives this keymap
/// together with the synthetic events.
pub fn plan(chord: &Chord) -> Result<ChordPlan, PlanError> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        "evdev",
        "pc105",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or(PlanError::Keymap)?;

    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();
    let xkb_key = (min..=max)
        .find(|&code| {
            keymap
                .key_get_syms_by_level(code.into(), 0, 0)
                .contains(&chord.keysym)
        })
        .ok_or_else(|| PlanError::KeyNotFound(chord.to_string()))?;

    let mut mods = 0u32;
    for m in &chord.modifiers {
        let index = keymap.mod_get_index(m.xkb_name());
        if index == xkb::MOD_INVALID {
            return Err(PlanError::ModifierNotFound(m.xkb_name()));
        }
        mods |= 1 << index;
    }

    let mut text = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    text.push(0);

    Ok(ChordPlan {
        keymap: text,
        key: xkb_key - 8,
        mods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chords() {
        let c: Chord = "ctrl+v".parse().unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Ctrl]);
        assert_eq!(c.keysym, xkb::Keysym::v);

        let c: Chord = "Control + Shift + V".parse().unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(c.keysym, xkb::Keysym::v);

        let c: Chord = "shift+Insert".parse().unwrap();
        assert_eq!(c.keysym, xkb::Keysym::Insert);
    }

    #[test]
    fn rejects_bad_chords() {
        assert_eq!("".parse::<Chord>(), Err(ChordError::Empty));
        assert_eq!("ctrl+".parse::<Chord>(), Err(ChordError::Empty));
        assert!(matches!(
            "hyper+v".parse::<Chord>(),
            Err(ChordError::UnknownModifier(_))
        ));
        assert!(matches!(
            "ctrl+ctrl+v".parse::<Chord>(),
            Err(ChordError::DuplicateModifier(_))
        ));
        assert!(matches!(
            "ctrl+notakey".parse::<Chord>(),
            Err(ChordError::UnknownKey(_))
        ));
    }

    #[test]
    fn plans_against_us_keymap() {
        let p = plan(&"ctrl+v".parse().unwrap()).unwrap();
        assert_eq!(p.key, 47); // KEY_V
        assert_ne!(p.mods, 0);
        assert_eq!(p.keymap.last(), Some(&0));

        let p2 = plan(&"ctrl+shift+v".parse().unwrap()).unwrap();
        assert_eq!(p2.key, 47);
        assert_ne!(p2.mods, p.mods);

        let p = plan(&"shift+Insert".parse().unwrap()).unwrap();
        assert_eq!(p.key, 110); // KEY_INSERT
    }
}
