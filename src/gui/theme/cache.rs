//! The resources a themed paint draws with, created on first use and kept until the theme
//! changes: the brushes and pens, the title font and the artwork decoded and scaled to the page.

use super::{
    ART_MARGIN,
    palette::{Edge, Fill, Palette, colour, palette},
};
use crate::{
    config::Theme,
    gui::art::{Image, Layer},
    sys::window,
};
use winsafe::{self as w, co, guard::DeleteObjectGuard, gui};

const TITLE_FONT_PERCENT: i32 = 118;

struct Artwork {
    character: &'static [u8],
    decoration: &'static [u8],
}

const fn artwork(theme: Theme) -> Option<Artwork> {
    match theme {
        Theme::Classic => None,
        Theme::Ember => Some(Artwork {
            character: include_bytes!("../../../assets/themes/ember/ember-character.png"),
            decoration: include_bytes!("../../../assets/themes/ember/ember-decoration.png"),
        }),
        Theme::Selene => Some(Artwork {
            character: include_bytes!("../../../assets/themes/selene/selene-character.png"),
            decoration: include_bytes!("../../../assets/themes/selene/selene-decoration.png"),
        }),
        Theme::Nova => Some(Artwork {
            character: include_bytes!("../../../assets/themes/nova/nova-character.png"),
            decoration: include_bytes!("../../../assets/themes/nova/nova-decoration.png"),
        }),
    }
}

#[derive(Default)]
pub(super) struct Resources {
    pub(super) gdi: Option<Gdi>,
    pictures: Option<Pictures>,
    scaled: Option<Scaled>,
    title_font: Option<DeleteObjectGuard<w::HFONT>>,
}

/// The resources one themed paint of a page borrows.
pub(super) struct Ready<'a> {
    pub(super) gdi: &'a Gdi,
    pub(super) scaled: Option<&'a Scaled>,
    pub(super) title_font: Option<&'a w::HFONT>,
}

impl Resources {
    pub(super) fn ready(
        &mut self,
        theme: Theme,
        page: &w::HWND,
        page_height: i32,
    ) -> Option<Ready<'_>> {
        let palette = palette(theme)?;
        self.ensure_title_font(page);
        self.ensure_scaled(theme, page_height);
        self.ensure_gdi(palette);
        Some(Ready {
            gdi: self.gdi.as_ref()?,
            scaled: self.scaled.as_ref(),
            title_font: self.title_font.as_deref(),
        })
    }

    pub(super) fn gdi(&mut self, palette: &Palette) -> Option<&Gdi> {
        self.ensure_gdi(palette);
        self.gdi.as_ref()
    }

    pub(super) fn ensure_gdi(&mut self, palette: &Palette) {
        if self.gdi.is_none() {
            self.gdi = Gdi::new(palette);
        }
    }

    /// Drops the resources that belong to the old theme. The title font follows the controls, not
    /// the theme.
    pub(super) fn forget_theme(&mut self) {
        self.gdi = None;
        self.pictures = None;
        self.scaled = None;
    }

    fn ensure_title_font(&mut self, window: &w::HWND) {
        if self.title_font.is_some() {
            return;
        }
        let Some(base) = window::control_font(window).and_then(|font| font.GetObject().ok()) else {
            return;
        };
        let mut font = base;
        font.lfHeight = font.lfHeight * TITLE_FONT_PERCENT / 100;
        font.lfWeight = co::FW::SEMIBOLD;
        self.title_font = w::HFONT::CreateFontIndirect(&font).ok();
    }

    fn ensure_scaled(&mut self, theme: Theme, page_height: i32) {
        if page_height <= 0 {
            return;
        }
        if self
            .scaled
            .as_ref()
            .is_some_and(|scaled| scaled.page_height == page_height)
        {
            return;
        }
        if self.pictures.is_none() {
            self.pictures = Pictures::decode(theme);
        }
        let Some(pictures) = &self.pictures else {
            return;
        };
        self.scaled = Scaled::new(pictures, page_height);
    }
}

/// A brush for every `Fill` and a pen for every `Edge`, so that no lookup can fail.
pub(super) struct Gdi {
    background: DeleteObjectGuard<w::HBRUSH>,
    panel: DeleteObjectGuard<w::HBRUSH>,
    secondary_button: DeleteObjectGuard<w::HBRUSH>,
    accent: DeleteObjectGuard<w::HBRUSH>,
    pressed: DeleteObjectGuard<w::HBRUSH>,
    hot: DeleteObjectGuard<w::HBRUSH>,
    border_pen: DeleteObjectGuard<w::HPEN>,
    accent_pen: DeleteObjectGuard<w::HPEN>,
}

impl Gdi {
    fn new(palette: &Palette) -> Option<Self> {
        let brush = |fill| solid_brush(palette.fill(fill)).ok();
        let pen = |edge| solid_pen(palette.edge(edge)).ok();
        Some(Self {
            background: brush(Fill::Background)?,
            panel: brush(Fill::Panel)?,
            secondary_button: brush(Fill::SecondaryButton)?,
            accent: brush(Fill::Accent)?,
            pressed: brush(Fill::Pressed)?,
            hot: brush(Fill::Hot)?,
            border_pen: pen(Edge::Border)?,
            accent_pen: pen(Edge::Accent)?,
        })
    }

    pub(super) fn brush(&self, fill: Fill) -> &w::HBRUSH {
        match fill {
            Fill::Background => &self.background,
            Fill::Panel => &self.panel,
            Fill::SecondaryButton => &self.secondary_button,
            Fill::Accent => &self.accent,
            Fill::Pressed => &self.pressed,
            Fill::Hot => &self.hot,
        }
    }

    pub(super) fn pen(&self, edge: Edge) -> &w::HPEN {
        match edge {
            Edge::Border => &self.border_pen,
            Edge::Accent => &self.accent_pen,
        }
    }
}

struct Pictures {
    character: Image,
    decoration: Image,
}

impl Pictures {
    fn decode(theme: Theme) -> Option<Self> {
        let artwork = artwork(theme)?;
        Some(Self {
            character: Image::decode(artwork.character)?,
            decoration: Image::decode(artwork.decoration)?,
        })
    }
}

pub(super) struct Scaled {
    page_height: i32,
    pub(super) character: Layer,
    pub(super) decoration: Layer,
}

impl Scaled {
    fn new(pictures: &Pictures, page_height: i32) -> Option<Self> {
        let margin = gui::dpi_y(ART_MARGIN);
        let character_height = usize::try_from(page_height - 2 * margin).unwrap_or(1);
        let decoration_height = usize::try_from(page_height).unwrap_or(1);
        Some(Self {
            page_height,
            character: Layer::new(&pictures.character.scaled_to_height(character_height))?,
            decoration: Layer::new(&pictures.decoration.scaled_to_height(decoration_height))?,
        })
    }
}

fn solid_brush(hex: u32) -> w::SysResult<DeleteObjectGuard<w::HBRUSH>> {
    w::HBRUSH::CreateSolidBrush(colour(hex))
}

fn solid_pen(hex: u32) -> w::SysResult<DeleteObjectGuard<w::HPEN>> {
    w::HPEN::CreatePen(co::PS::SOLID, 1, colour(hex))
}
