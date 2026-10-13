//! Each look's colours: the character themes' palettes and the classic look's tints.

use super::Tone;
use crate::config::Theme;
use winsafe as w;

const PRESSED_PRIMARY: u32 = 40;
const HOT_PRIMARY: u32 = 28;
const BLACK_RGB: u32 = 0x0000_0000;

/// The classic look's tints, on the system's window colours.
const CLASSIC_MUTED: u32 = 0x006D_6D6D;
const CLASSIC_GOOD: u32 = 0x0010_7C10;
const CLASSIC_WARNING: u32 = 0x009D_5D00;
const CLASSIC_ERROR: u32 = 0x00C4_2B1C;

/// The classic look's tint for a tone; `None` keeps the system's text colour.
pub(super) const fn classic_tint(tone: Tone) -> Option<u32> {
    match tone {
        Tone::Text => None,
        Tone::Muted => Some(CLASSIC_MUTED),
        Tone::Good => Some(CLASSIC_GOOD),
        Tone::Warning => Some(CLASSIC_WARNING),
        Tone::Error => Some(CLASSIC_ERROR),
    }
}

pub(super) struct Palette {
    pub(super) background: u32,
    pub(super) panel: u32,
    pub(super) border: u32,
    pub(super) accent: u32,
    pub(super) text: u32,
    pub(super) muted: u32,
    pub(super) status: u32,
    pub(super) warning: u32,
    pub(super) error: u32,
    pub(super) on_accent: u32,
    pub(super) secondary_button: u32,
}

const EMBER: Palette = Palette {
    background: 0x00FB_FAF4,
    panel: 0x00FF_FEFA,
    border: 0x00C5_D1BC,
    accent: 0x0031_5D45,
    text: 0x001F_2A23,
    muted: 0x0065_7367,
    status: 0x0023_821C,
    warning: CLASSIC_WARNING,
    error: CLASSIC_ERROR,
    on_accent: 0x00FF_FFFF,
    secondary_button: 0x00F6_F5EF,
};

const SELENE: Palette = Palette {
    background: 0x00F8_F6FC,
    panel: 0x00FF_FEFF,
    border: 0x00D1_C5E0,
    accent: 0x0063_5082,
    text: 0x0029_2535,
    muted: 0x0070_667E,
    status: 0x0023_821C,
    warning: CLASSIC_WARNING,
    error: CLASSIC_ERROR,
    on_accent: 0x00FF_FFFF,
    secondary_button: 0x00F4_F0F8,
};

const NOVA: Palette = Palette {
    background: 0x00F3_F8FC,
    panel: 0x00FF_FFFF,
    border: 0x00BD_D1E3,
    accent: 0x0028_5B92,
    text: 0x001F_2C3A,
    muted: 0x0060_7386,
    status: 0x0023_821C,
    warning: CLASSIC_WARNING,
    error: CLASSIC_ERROR,
    on_accent: 0x00FF_FFFF,
    secondary_button: 0x00ED_F3F9,
};

/// Every colour a theme fills with; the cache holds a brush for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fill {
    Background,
    Panel,
    SecondaryButton,
    Accent,
    Pressed,
    Hot,
}

/// Every colour a theme outlines with; the cache holds a pen for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Border,
    Accent,
}

impl Palette {
    pub(super) fn fill(&self, fill: Fill) -> u32 {
        match fill {
            Fill::Background => self.background,
            Fill::Panel => self.panel,
            Fill::SecondaryButton => self.secondary_button,
            Fill::Accent => self.accent,
            Fill::Pressed => self.pressed(),
            Fill::Hot => self.hot(),
        }
    }

    pub(super) const fn edge(&self, edge: Edge) -> u32 {
        match edge {
            Edge::Border => self.border,
            Edge::Accent => self.accent,
        }
    }

    pub(super) const fn tone(&self, tone: Tone) -> u32 {
        match tone {
            Tone::Text => self.text,
            Tone::Muted => self.muted,
            Tone::Good => self.status,
            Tone::Warning => self.warning,
            Tone::Error => self.error,
        }
    }

    fn pressed(&self) -> u32 {
        mix(self.accent, BLACK_RGB, PRESSED_PRIMARY)
    }

    fn hot(&self) -> u32 {
        mix(self.accent, self.on_accent, HOT_PRIMARY)
    }
}

pub(super) const fn palette(theme: Theme) -> Option<&'static Palette> {
    match theme {
        Theme::Classic => None,
        Theme::Ember => Some(&EMBER),
        Theme::Selene => Some(&SELENE),
        Theme::Nova => Some(&NOVA),
    }
}

pub(super) const fn colour(hex: u32) -> w::COLORREF {
    let [_, red, green, blue] = hex.to_be_bytes();
    w::COLORREF::from_rgb(red, green, blue)
}

fn mix(from: u32, to: u32, part: u32) -> u32 {
    let channel = |shift: u32| {
        let (a, b) = ((from >> shift) & 0xFF, (to >> shift) & 0xFF);
        (a * (255 - part) + b * part + 127) / 255
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_moves_each_channel_part_of_the_way_and_rounds_to_nearest() {
        assert_eq!(mix(0x0010_2030, 0x00FF_FFFF, 0), 0x0010_2030);
        assert_eq!(mix(0x0010_2030, 0x00FF_FFFF, 255), 0x00FF_FFFF);
        assert_eq!(mix(BLACK_RGB, 0x00FF_FFFF, 128), 0x0080_8080);
        assert_eq!(mix(0x0000_0000, 0x00FF_0000, 51), 0x0033_0000);
    }

    #[test]
    fn a_colour_keeps_its_channels_in_the_order_windows_wants() {
        let colour = colour(0x0012_3456);
        assert_eq!(
            (colour.GetRValue(), colour.GetGValue(), colour.GetBValue()),
            (0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn a_pressed_button_darkens_its_accent_and_a_hot_one_lightens_it() {
        assert_eq!(
            EMBER.fill(Fill::Pressed),
            mix(EMBER.accent, BLACK_RGB, PRESSED_PRIMARY)
        );
        assert_eq!(
            EMBER.fill(Fill::Hot),
            mix(EMBER.accent, EMBER.on_accent, HOT_PRIMARY)
        );
    }
}
