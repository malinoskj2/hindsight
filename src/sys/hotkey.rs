//! The global hotkey that saves a clip. It is written as text such as `Alt+F10` in the settings
//! and registered with Windows for the application window, so it fires while a game has focus.

use std::{fmt, str::FromStr};
use thiserror::Error;

const MOD_ALT: u16 = 0x0001;
const MOD_CONTROL: u16 = 0x0002;
const MOD_SHIFT: u16 = 0x0004;
const MOD_WIN: u16 = 0x0008;
/// Modifiers in the order they are written.
const MODIFIERS: [(u16, &str); 4] = [
    (MOD_CONTROL, "Ctrl"),
    (MOD_ALT, "Alt"),
    (MOD_SHIFT, "Shift"),
    (MOD_WIN, "Win"),
];

const VK_F1: u16 = 0x70;
/// Function keys run from F1 to F24.
const FUNCTION_KEY_COUNT: u16 = 24;
const VK_NUMPAD0: u16 = 0x60;
/// Number-pad digits run from Numpad0 to Numpad9.
const NUMPAD_DIGIT_COUNT: u16 = 10;
const VK_PRINT_SCREEN: u16 = 0x2C;
const VK_PAUSE: u16 = 0x13;
const VK_SCROLL_LOCK: u16 = 0x91;
/// Named keys other than letters, digits, function keys and the number pad.
const NAMED_KEYS: [(u16, &str); 11] = [
    (0x20, "Space"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x24, "Home"),
    (0x23, "End"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (VK_PRINT_SCREEN, "PrintScreen"),
    (VK_PAUSE, "Pause"),
    (VK_SCROLL_LOCK, "ScrollLock"),
    (0xC0, "`"),
];
/// Keys that may stand alone: pressing them never types text.
const STANDALONE_KEYS: [u16; 3] = [VK_PRINT_SCREEN, VK_PAUSE, VK_SCROLL_LOCK];

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("enter a hotkey such as Alt+F10")]
    Empty,
    #[error("\"{0}\" is not a key the hotkey can use")]
    UnknownKey(String),
    #[error("\"{0}\" appears twice in the hotkey")]
    Repeated(String),
    #[error("the hotkey needs Ctrl, Alt, Shift or Win with {0}")]
    NeedsModifier(String),
}

/// Modifiers (`MOD_*` flags) and a virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hotkey {
    modifiers: u16,
    key: u16,
}

impl FromStr for Hotkey {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split('+').map(str::trim);
        let key_name = parts
            .next_back()
            .filter(|name| !name.is_empty())
            .ok_or(ParseError::Empty)?;

        let modifiers = parse_modifiers(parts)?;
        let key = key_code(key_name).ok_or_else(|| ParseError::UnknownKey(key_name.to_owned()))?;
        if modifiers == 0 && !may_stand_alone(key) {
            return Err(ParseError::NeedsModifier(key_label(key)));
        }

        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (flag, name) in MODIFIERS {
            if self.modifiers & flag != 0 {
                write!(formatter, "{name}+")?;
            }
        }
        formatter.write_str(&key_label(self.key))
    }
}

/// The `MOD_*` flags of the modifier names, each of which may appear once.
fn parse_modifiers<'a>(names: impl Iterator<Item = &'a str>) -> Result<u16, ParseError> {
    let mut modifiers = 0;
    for name in names {
        let flag = modifier(name).ok_or_else(|| ParseError::UnknownKey(name.to_owned()))?;
        if modifiers & flag != 0 {
            return Err(ParseError::Repeated(name.to_owned()));
        }
        modifiers |= flag;
    }
    Ok(modifiers)
}

/// The `MOD_*` flag of a modifier name, in any case.
fn modifier(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(MOD_CONTROL),
        "alt" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "win" | "windows" => Some(MOD_WIN),
        _ => None,
    }
}

/// The virtual-key code of a key name, in any case.
fn key_code(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    letter_or_digit(&upper)
        .or_else(|| function_key(&upper))
        .or_else(|| numpad_digit(&upper))
        .or_else(|| named_key(name))
}

/// The code of a single letter or digit, which is its upper-case character.
fn letter_or_digit(upper: &str) -> Option<u16> {
    let mut chars = upper.chars();
    match (chars.next(), chars.next()) {
        (Some(single), None) if single.is_ascii_alphanumeric() => {
            u16::try_from(u32::from(single)).ok()
        }
        _ => None,
    }
}

/// The code of `F1` to `F24`.
fn function_key(upper: &str) -> Option<u16> {
    upper
        .strip_prefix('F')
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|number| (1..=FUNCTION_KEY_COUNT).contains(number))
        .map(|number| VK_F1 + number - 1)
}

/// The code of `Numpad0` to `Numpad9`.
fn numpad_digit(upper: &str) -> Option<u16> {
    upper
        .strip_prefix("NUMPAD")
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|digit| *digit < NUMPAD_DIGIT_COUNT)
        .map(|digit| VK_NUMPAD0 + digit)
}

/// The code of one of `NAMED_KEYS`.
fn named_key(name: &str) -> Option<u16> {
    NAMED_KEYS
        .iter()
        .find(|(_, named)| named.eq_ignore_ascii_case(name))
        .map(|(code, _)| *code)
}

/// Whether a key may be the hotkey without a modifier: pressing it never types text.
fn may_stand_alone(key: u16) -> bool {
    is_function_key(key) || STANDALONE_KEYS.contains(&key)
}

/// The name of a key as the hotkey is written, the reverse of `key_code`.
fn key_label(key: u16) -> String {
    if is_function_key(key) {
        return format!("F{}", key - VK_F1 + 1);
    }
    if is_numpad_digit(key) {
        return format!("Numpad{}", key - VK_NUMPAD0);
    }
    if let Some((_, name)) = NAMED_KEYS.iter().find(|(code, _)| *code == key) {
        return (*name).to_owned();
    }
    char::from_u32(u32::from(key)).map_or_else(|| format!("{key:#04x}"), String::from)
}

/// Whether a key is one of `F1` to `F24`.
const fn is_function_key(key: u16) -> bool {
    key >= VK_F1 && key < VK_F1 + FUNCTION_KEY_COUNT
}

/// Whether a key is one of `Numpad0` to `Numpad9`.
const fn is_numpad_digit(key: u16) -> bool {
    key >= VK_NUMPAD0 && key < VK_NUMPAD0 + NUMPAD_DIGIT_COUNT
}

#[cfg(windows)]
pub(crate) use native::{SAVE_HOTKEY_ID, register};

#[cfg(windows)]
mod native {
    use super::Hotkey;
    use thiserror::Error;
    use winsafe::{self as w, co};

    /// The `WM_HOTKEY` identifier of the save hotkey.
    pub(crate) const SAVE_HOTKEY_ID: i32 = 1;

    #[derive(Debug, Error)]
    pub(crate) enum RegisterError {
        #[error("{0} is already used by another application; choose another hotkey in Settings")]
        Taken(Hotkey),
        #[error("register the {0} hotkey: {1}")]
        System(Hotkey, co::ERROR),
    }

    /// Makes `hotkey` save a clip, replacing the hotkey registered before. Held keys do not
    /// repeat it.
    pub(crate) fn register(window: &w::HWND, hotkey: Hotkey) -> Result<(), RegisterError> {
        let (modifiers, key) = system_codes(hotkey);
        // Nothing may be registered yet; that is not an error.
        let _ = window.UnregisterHotKey(SAVE_HOTKEY_ID);
        window
            .RegisterHotKey(SAVE_HOTKEY_ID, modifiers, key)
            .map_err(|error| {
                if error == co::ERROR::HOTKEY_ALREADY_REGISTERED {
                    RegisterError::Taken(hotkey)
                } else {
                    RegisterError::System(hotkey, error)
                }
            })
    }

    /// The hotkey's modifiers, with `NOREPEAT` added, and key as the types `RegisterHotKey`
    /// takes.
    #[expect(unsafe_code)]
    fn system_codes(hotkey: Hotkey) -> (co::MOD, co::VK) {
        // SAFETY: `MOD` and `VK` are plain newtypes over the flag and key values; every value
        // the parser produces is representable.
        unsafe {
            (
                co::MOD::from_raw(hotkey.modifiers) | co::MOD::NOREPEAT,
                co::VK::from_raw(hotkey.key),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn parse(text: &str) -> Result<Hotkey, ParseError> {
        text.parse()
    }

    #[test]
    fn hotkeys_parse_modifiers_and_keys_in_any_case_and_order() {
        assert_eq!(
            parse("Alt+F10").unwrap(),
            Hotkey {
                modifiers: MOD_ALT,
                key: 0x79
            }
        );
        assert_eq!(
            parse(" shift + CTRL + s ").unwrap(),
            Hotkey {
                modifiers: MOD_CONTROL | MOD_SHIFT,
                key: u16::from(b'S')
            }
        );
        assert_eq!(parse("Win+Numpad7").unwrap().key, VK_NUMPAD0 + 7);
        assert_eq!(parse("Ctrl+PageDown").unwrap().key, 0x22);
        assert_eq!(parse("F24").unwrap().key, VK_F1 + 23);
        assert_eq!(parse("Pause").unwrap().modifiers, 0);
    }

    #[test]
    fn hotkeys_display_their_canonical_spelling() {
        for (text, canonical) in [
            ("alt+f10", "Alt+F10"),
            ("Shift+Win+Ctrl+Alt+9", "Ctrl+Alt+Shift+Win+9"),
            ("control+numpad0", "Ctrl+Numpad0"),
            ("ALT+PRINTSCREEN", "Alt+PrintScreen"),
            ("Ctrl+`", "Ctrl+`"),
        ] {
            assert_eq!(parse(text).unwrap().to_string(), canonical);
            assert_eq!(parse(canonical).unwrap(), parse(text).unwrap());
        }
    }

    #[test]
    fn malformed_hotkeys_are_rejected() {
        assert_matches!(parse(""), Err(ParseError::Empty));
        assert_matches!(parse("Alt+"), Err(ParseError::Empty));
        assert_matches!(parse("Alt+F25"), Err(ParseError::UnknownKey(_)));
        assert_matches!(parse("Hyper+F1"), Err(ParseError::UnknownKey(_)));
        assert_matches!(parse("Alt+Alt+F1"), Err(ParseError::Repeated(_)));
        assert_matches!(parse("S"), Err(ParseError::NeedsModifier(_)));
        assert_matches!(parse("Space"), Err(ParseError::NeedsModifier(_)));
        assert_eq!(
            parse("q").unwrap_err().to_string(),
            "the hotkey needs Ctrl, Alt, Shift or Win with Q"
        );
    }
}
