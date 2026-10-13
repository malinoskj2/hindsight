//! The window's look: classic leaves native drawing alone; a character theme paints over the same controls.
//!
//! The colours live in `palette`, what a paint draws with in `cache`, and the painters in
//! `paint`; `Skin` ties them to the current theme.

use self::{
    cache::{Ready, Resources},
    paint::{
        buffered, button_colours, draw_centred, draw_focus, paint_frame, paint_tab_strip,
        round_rect,
    },
    palette::{Fill, Palette, classic_tint, colour, palette},
};
use super::layout::Frame;
use crate::{config::Theme, sys::window};
use std::{
    cell::{Cell, Ref, RefCell},
    rc::Rc,
};
use winsafe::{self as w, co, gui, prelude::*};

pub(super) use self::paint::paint_tabs_natively;

mod cache;
mod paint;
mod palette;

const ART_WIDTH: i32 = 240;
const ART_MARGIN: i32 = 8;
const DECORATION_OPACITY: u8 = 166;

/// What a label's text says; each look gives every tone a colour of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Text,
    Muted,
    Good,
    Warning,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Surface {
    Dialog,
    Page,
}

impl Surface {
    const fn fill(self) -> Fill {
        match self {
            Self::Dialog => Fill::Background,
            Self::Page => Fill::Panel,
        }
    }

    pub(super) const fn classic(self) -> co::COLOR {
        match self {
            Self::Dialog => co::COLOR::BTNFACE,
            Self::Page => co::COLOR::WINDOW,
        }
    }
}

#[derive(Clone)]
pub(super) struct Skin(Rc<Inner>);

struct Inner {
    theme: Cell<Theme>,
    resources: RefCell<Resources>,
}

impl Skin {
    pub(super) fn new(theme: Theme) -> Self {
        Self(Rc::new(Inner {
            theme: Cell::new(theme),
            resources: RefCell::default(),
        }))
    }

    pub(super) fn theme(&self) -> Theme {
        self.0.theme.get()
    }

    pub(super) fn set(&self, theme: Theme) {
        self.0.theme.set(theme);
        self.0.resources.borrow_mut().forget_theme();
    }

    pub(super) fn is_themed(&self) -> bool {
        self.palette().is_some()
    }

    pub(super) fn extra_width(&self) -> i32 {
        if self.is_themed() { ART_WIDTH } else { 0 }
    }

    /// Classic text takes the system's colour, as does every tone under high contrast: the
    /// labels sit on the system's background, which such a theme may make black.
    pub(super) fn colour(&self, tone: Tone) -> w::COLORREF {
        if let Some(palette) = self.palette() {
            return colour(palette.tone(tone));
        }
        match classic_tint(tone) {
            Some(tint) if !window::high_contrast() => colour(tint),
            _ => w::GetSysColor(co::COLOR::WINDOWTEXT),
        }
    }

    /// Caption then text; `None` in classic.
    pub(super) fn title_bar_colours(&self) -> Option<(w::COLORREF, w::COLORREF)> {
        self.palette()
            .map(|palette| (colour(palette.background), colour(palette.text)))
    }

    /// Background then text; `None` in classic.
    pub(super) fn list_colours(&self) -> Option<(w::COLORREF, w::COLORREF)> {
        self.palette()
            .map(|palette| (colour(palette.panel), colour(palette.text)))
    }

    /// For `WM_CTLCOLORSTATIC`, which hands the brush to the label by value.
    #[expect(unsafe_code)]
    pub(super) fn label_brush(&self, surface: Surface) -> w::HBRUSH {
        match self.brush(surface) {
            // SAFETY: the label paints with the copy as soon as this message returns, and the
            // skin drops its brushes only on a theme change, never during a paint.
            Some(brush) => unsafe { brush.raw_copy() },
            None => w::HBRUSH::GetSysColorBrush(surface.classic()).unwrap_or(w::HBRUSH::NULL),
        }
    }

    pub(super) fn fill_dialog(&self, hdc: &w::HDC, dialog: &w::HWND) {
        let Ok(client) = dialog.GetClientRect() else {
            return;
        };
        if let Some(background) = self.brush(Surface::Dialog) {
            let _ = hdc.FillRect(client, &background);
        }
    }

    pub(super) fn paint_backdrop(&self, hdc: &w::HDC, page: &w::HWND, frames: &[Frame]) {
        let Some(palette) = self.palette() else {
            return;
        };
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        let mut resources = self.0.resources.borrow_mut();
        let Some(ready) = resources.ready(self.theme(), page, client.bottom - client.top) else {
            return;
        };
        let _ = hdc.FillRect(client, ready.gdi.brush(Fill::Background));
        if let Some(scaled) = ready.scaled {
            let (width, height) = scaled.decoration.size();
            scaled.decoration.draw(
                hdc,
                client.right - width,
                client.bottom - height,
                DECORATION_OPACITY,
            );
        }
        for frame in frames {
            paint_frame(hdc, palette, ready.gdi, ready.title_font, frame);
        }
    }

    pub(super) fn paint_page(&self, hdc: &w::HDC, page: &w::HWND, frames: &[Frame]) {
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        buffered(hdc, (client.right, client.bottom), None, |target| {
            self.paint_backdrop(target, page, frames);
            self.paint_character(target, page);
        });
    }

    fn paint_character(&self, hdc: &w::HDC, page: &w::HWND) {
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        let mut resources = self.0.resources.borrow_mut();
        let Some(Ready {
            scaled: Some(scaled),
            ..
        }) = resources.ready(self.theme(), page, client.bottom - client.top)
        else {
            return;
        };
        let (width, height) = scaled.character.size();
        let margin = gui::dpi_x(ART_MARGIN);
        scaled.character.draw(
            hdc,
            client.right - width - margin,
            client.bottom - height - margin,
            u8::MAX,
        );
    }

    pub(super) fn paint_button(&self, draw: &w::NMCUSTOMDRAW, text: &str) -> co::CDRF {
        let Some(palette) = self.palette() else {
            return co::CDRF::DODEFAULT;
        };
        if draw.dwDrawStage != co::CDDS::PREPAINT {
            return co::CDRF::DODEFAULT;
        }
        let state = draw.uItemState;
        let (fill, edge, ink) = button_colours(palette, state);

        let hdc = &draw.hdc;
        let mut resources = self.0.resources.borrow_mut();
        let Some(gdi) = resources.gdi(palette) else {
            return co::CDRF::DODEFAULT;
        };
        // Behind the rounded corners: the panel the button sits on.
        let _ = hdc.FillRect(draw.rc, gdi.brush(Fill::Panel));
        if !round_rect(hdc, draw.rc, gdi.pen(edge), gdi.brush(fill)) {
            return co::CDRF::DODEFAULT;
        }
        let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);
        let _ = hdc.SetTextColor(colour(ink));
        draw_centred(hdc, text, draw.rc);
        if state.has(co::CDIS::FOCUS) && state.has(co::CDIS::SHOWKEYBOARDCUES) {
            draw_focus(hdc, draw.rc);
        }
        co::CDRF::SKIPDEFAULT
    }

    pub(super) fn paint_tabs(&self, tab: &gui::Tab) {
        let Some(palette) = self.palette() else {
            return;
        };
        let hwnd = tab.hwnd();
        let Ok(paint) = hwnd.BeginPaint() else {
            return;
        };
        let Ok(client) = hwnd.GetClientRect() else {
            return;
        };
        let mut resources = self.0.resources.borrow_mut();
        let Some(gdi) = resources.gdi(palette) else {
            return;
        };
        // The pages paint the display area themselves; keep out of it.
        let pages = window::display_rect(hwnd, client);
        buffered(&paint, (client.right, client.bottom), pages, |target| {
            paint_tab_strip(target, palette, gdi, tab, client);
        });
    }

    fn palette(&self) -> Option<&'static Palette> {
        palette(self.theme())
    }

    /// The surface's brush, borrowed from the skin's resources; `None` in classic.
    fn brush(&self, surface: Surface) -> Option<Ref<'_, w::HBRUSH>> {
        let palette = self.palette()?;
        self.0.resources.borrow_mut().ensure_gdi(palette);
        Ref::filter_map(self.0.resources.borrow(), |resources| {
            resources.gdi.as_ref().map(|gdi| gdi.brush(surface.fill()))
        })
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_tone_has_a_colour_of_its_own_in_every_look() {
        let tones = [
            Tone::Text,
            Tone::Muted,
            Tone::Good,
            Tone::Warning,
            Tone::Error,
        ];
        for theme in [Theme::Ember, Theme::Selene, Theme::Nova] {
            let skin = Skin::new(theme);
            let colours: HashSet<u32> = tones.map(|tone| skin.colour(tone).raw()).into();
            assert_eq!(colours.len(), tones.len(), "{theme:?}");
        }
        let tints: HashSet<u32> = tones.into_iter().filter_map(classic_tint).collect();
        assert_eq!(tints.len(), tones.len() - 1);
        assert_eq!(
            classic_tint(Tone::Text),
            None,
            "classic text is the system's"
        );
        let ember = palette(Theme::Ember).unwrap();
        assert_eq!(
            Skin::new(Theme::Ember).colour(Tone::Muted).raw(),
            colour(ember.muted).raw()
        );
    }

    #[test]
    fn a_list_takes_the_panel_colours_in_a_theme_and_none_in_classic() {
        let raw = |colours: Option<(w::COLORREF, w::COLORREF)>| {
            colours.map(|(background, text)| (background.raw(), text.raw()))
        };
        let ember = palette(Theme::Ember).unwrap();
        assert_eq!(raw(Skin::new(Theme::Classic).list_colours()), None);
        assert_eq!(
            raw(Skin::new(Theme::Ember).list_colours()),
            Some((colour(ember.panel).raw(), colour(ember.text).raw()))
        );
    }
}
