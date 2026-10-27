//! The global hotkeys that save a clip and take a screenshot, written as text such as
//! `Alt+F7`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, ops::BitOr, str::FromStr};
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
const FUNCTION_KEY_COUNT: u16 = 24;
const VK_NUMPAD0: u16 = 0x60;
const NUMPAD_DIGIT_COUNT: u16 = 10;
const VK_PRINT_SCREEN: u16 = 0x2C;
const VK_PAUSE: u16 = 0x13;
const VK_SCROLL_LOCK: u16 = 0x91;
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
/// Keys that never type text when pressed, so they may be used without a modifier.
const STANDALONE_KEYS: [u16; 3] = [VK_PRINT_SCREEN, VK_PAUSE, VK_SCROLL_LOCK];

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("enter a hotkey such as Alt+F7")]
    Empty,
    #[error("\"{0}\" is not a key the hotkey can use")]
    UnknownKey(String),
    #[error("\"{0}\" appears twice in the hotkey")]
    Repeated(String),
    #[error("the hotkey needs Ctrl, Alt, Shift or Win with {0}")]
    NeedsModifier(String),
}

/// A hotkey, serialised and displayed as text, so the settings file contains `"Alt+F7"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hotkey {
    modifiers: u16,
    key: u16,
}

/// The modifier keys held down while another key is pressed, combined with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Modifiers(u16);

impl Modifiers {
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const CTRL: Self = Self(MOD_CONTROL);
    pub(crate) const ALT: Self = Self(MOD_ALT);
    pub(crate) const SHIFT: Self = Self(MOD_SHIFT);
    pub(crate) const WIN: Self = Self(MOD_WIN);
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl Hotkey {
    /// Alt+F7 and Alt+F8 stay clear of the NVIDIA overlay's Alt+F1, F2, F3, F9, F10 and F12, and
    /// of F12, which `RegisterHotKey` reserves for debuggers.
    pub(crate) const DEFAULT: Self = Self {
        modifiers: MOD_ALT,
        key: VK_F1 + 6,
    };
    pub(crate) const DEFAULT_SCREENSHOT: Self = Self {
        modifiers: MOD_ALT,
        key: VK_F1 + 7,
    };

    /// Returns the hotkey a hotkey field enters when `key`, a virtual-key code, is pressed with
    /// `modifiers` held, or `None` when the press is typing or editing instead. Shift alone is not
    /// enough, so capitals still type and Shift+Home still selects.
    pub(crate) fn pressed(modifiers: Modifiers, key: u16) -> Option<Self> {
        let Modifiers(modifiers) = modifiers;
        let held = modifiers & !MOD_SHIFT != 0;
        (is_known_key(key) && (held || may_stand_alone(key))).then_some(Self { modifiers, key })
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
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

fn modifier(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(MOD_CONTROL),
        "alt" => Some(MOD_ALT),
        "shift" => Some(MOD_SHIFT),
        "win" | "windows" => Some(MOD_WIN),
        _ => None,
    }
}

fn key_code(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    letter_or_digit(&upper)
        .or_else(|| function_key(&upper))
        .or_else(|| numpad_digit(&upper))
        .or_else(|| named_key(name))
}

fn letter_or_digit(upper: &str) -> Option<u16> {
    let mut chars = upper.chars();
    match (chars.next(), chars.next()) {
        (Some(single), None) if single.is_ascii_alphanumeric() => {
            u16::try_from(u32::from(single)).ok()
        }
        _ => None,
    }
}

fn function_key(upper: &str) -> Option<u16> {
    upper
        .strip_prefix('F')
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|number| (1..=FUNCTION_KEY_COUNT).contains(number))
        .map(|number| VK_F1 + number - 1)
}

fn numpad_digit(upper: &str) -> Option<u16> {
    upper
        .strip_prefix("NUMPAD")
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|digit| *digit < NUMPAD_DIGIT_COUNT)
        .map(|digit| VK_NUMPAD0 + digit)
}

fn named_key(name: &str) -> Option<u16> {
    NAMED_KEYS
        .iter()
        .find(|(_, named)| named.eq_ignore_ascii_case(name))
        .map(|(code, _)| *code)
}

fn is_known_key(key: u16) -> bool {
    u8::try_from(key).is_ok_and(|code| code.is_ascii_uppercase() || code.is_ascii_digit())
        || is_function_key(key)
        || is_numpad_digit(key)
        || NAMED_KEYS.iter().any(|(code, _)| *code == key)
}

fn may_stand_alone(key: u16) -> bool {
    is_function_key(key) || STANDALONE_KEYS.contains(&key)
}

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

const fn is_function_key(key: u16) -> bool {
    key >= VK_F1 && key < VK_F1 + FUNCTION_KEY_COUNT
}

const fn is_numpad_digit(key: u16) -> bool {
    key >= VK_NUMPAD0 && key < VK_NUMPAD0 + NUMPAD_DIGIT_COUNT
}

#[cfg(windows)]
pub(crate) use native::{Action, RegisterError, register, unregister};

#[cfg(windows)]
mod native {
    use super::Hotkey;
    use thiserror::Error;
    use winsafe::{self as w, co};

    /// What a hotkey does. Each is registered under an ID of its own.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Action {
        Save,
        Screenshot,
    }

    impl Action {
        pub(crate) const ALL: [Self; 2] = [Self::Save, Self::Screenshot];

        pub(super) const fn id(self) -> i32 {
            match self {
                Self::Save => 1,
                Self::Screenshot => 2,
            }
        }

        /// The action registered under the ID a `WM_HOTKEY` message carries.
        pub(crate) fn from_id(id: usize) -> Option<Self> {
            Self::ALL
                .into_iter()
                .find(|action| usize::try_from(action.id()) == Ok(id))
        }

        pub(crate) const fn index(self) -> usize {
            match self {
                Self::Save => 0,
                Self::Screenshot => 1,
            }
        }
    }

    #[derive(Debug, Error)]
    pub(crate) enum RegisterError {
        #[error("{0} is already used by another application; choose another hotkey in Settings")]
        Taken(Hotkey),
        #[error("register the {0} hotkey: {1}")]
        System(Hotkey, co::ERROR),
    }

    pub(crate) fn register(
        window: &w::HWND,
        action: Action,
        hotkey: Hotkey,
    ) -> Result<(), RegisterError> {
        let (modifiers, key) = system_codes(hotkey);
        let _ = window.UnregisterHotKey(action.id());
        window
            .RegisterHotKey(action.id(), modifiers, key)
            .map_err(|error| {
                if error == co::ERROR::HOTKEY_ALREADY_REGISTERED {
                    RegisterError::Taken(hotkey)
                } else {
                    RegisterError::System(hotkey, error)
                }
            })
    }

    pub(crate) fn unregister(window: &w::HWND, action: Action) {
        let _ = window.UnregisterHotKey(action.id());
    }

    #[expect(unsafe_code, reason = "builds winsafe's MOD and VK from raw values")]
    fn system_codes(hotkey: Hotkey) -> (co::MOD, co::VK) {
        // SAFETY: `MOD` is a plain newtype, and every parsed modifier set is representable.
        let modifiers = unsafe { co::MOD::from_raw(hotkey.modifiers) };
        // SAFETY: `VK` is a plain newtype, and every parsed key code is representable.
        let key = unsafe { co::VK::from_raw(hotkey.key) };
        (modifiers | co::MOD::NOREPEAT, key)
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
    fn the_default_hotkeys_are_alt_f7_and_alt_f8() {
        assert_eq!(Hotkey::DEFAULT.to_string(), "Alt+F7");
        assert_eq!(Hotkey::DEFAULT_SCREENSHOT.to_string(), "Alt+F8");
    }

    #[cfg(windows)]
    #[test]
    fn each_action_is_found_again_by_its_own_id() {
        for action in Action::ALL {
            assert_ne!(action.id(), 0);
            assert_eq!(
                Action::from_id(usize::try_from(action.id()).unwrap()),
                Some(action)
            );
            assert_eq!(Action::ALL[action.index()], action);
        }
        assert_eq!(Action::from_id(0), None);
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

    #[test]
    fn the_known_keys_are_those_whose_names_parse_back() {
        for key in 0..=u16::from(u8::MAX) {
            assert_eq!(
                is_known_key(key),
                key_code(&key_label(key)) == Some(key),
                "{key:#04x}"
            );
        }
    }

    #[test]
    fn a_pressed_hotkey_is_written_as_its_parser_reads_it() {
        let held = [
            Modifiers::NONE,
            Modifiers::CTRL,
            Modifiers::ALT | Modifiers::SHIFT,
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::WIN,
        ];
        for modifiers in held {
            for key in 0..=u16::from(u8::MAX) {
                if let Some(hotkey) = Hotkey::pressed(modifiers, key) {
                    assert_eq!(parse(&hotkey.to_string()).unwrap(), hotkey);
                }
            }
        }
    }

    #[test]
    fn presses_that_type_or_edit_are_not_taken_as_hotkeys() {
        const VK_BACK: u16 = 0x08;
        const VK_TAB: u16 = 0x09;
        const VK_SHIFT: u16 = 0x10;
        const VK_CONTROL: u16 = 0x11;
        const VK_HOME: u16 = 0x24;
        const VK_DELETE: u16 = 0x2E;
        const VK_OEM_PLUS: u16 = 0xBB;
        let ctrl_alt = Modifiers::CTRL | Modifiers::ALT;
        let shift = Modifiers::SHIFT;
        let nothing = Modifiers::NONE;
        let entered =
            |modifiers, key| Hotkey::pressed(modifiers, key).map(|hotkey| hotkey.to_string());

        assert_eq!(
            entered(ctrl_alt, VK_F1 + 9).as_deref(),
            Some("Ctrl+Alt+F10")
        );
        assert_eq!(entered(nothing, VK_F1 + 9).as_deref(), Some("F10"));
        assert_eq!(entered(shift, VK_F1 + 9).as_deref(), Some("Shift+F10"));
        assert_eq!(
            entered(nothing, VK_PRINT_SCREEN).as_deref(),
            Some("PrintScreen")
        );
        assert_eq!(
            entered(ctrl_alt, u16::from(b'S')).as_deref(),
            Some("Ctrl+Alt+S")
        );

        for (modifiers, key) in [
            (nothing, u16::from(b'A')),
            (shift, u16::from(b'A')),
            (shift, VK_HOME),
            (nothing, VK_DELETE),
            (nothing, VK_BACK),
            (shift, VK_TAB),
            (ctrl_alt, VK_TAB),
            (ctrl_alt, VK_CONTROL),
            (shift, VK_SHIFT),
            (shift, VK_OEM_PLUS),
        ] {
            assert_eq!(entered(modifiers, key), None, "{modifiers:?} {key:#04x}");
        }
    }
}
