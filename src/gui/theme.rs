//! The window's look. Classic leaves the system's own drawing alone; a character theme paints
//! its colours, group panels, tab strip, buttons and artwork over the same native controls, so
//! nothing about how they behave changes. One [`Skin`] is shared by every handler that paints,
//! and switching theme is just setting it and redrawing.
//!
//! Layers, bottom to top, as the artwork's spec lays them out: the flat background, a faint
//! ornament against the right edge, the group panels, then the character. The controls are
//! windows of their own and so sit above all of it; they stay clear of the artwork's column.

use super::{
    art::{Image, Layer},
    layout::Frame,
};
use crate::config::Theme;
use std::{cell::Cell, cell::RefCell, rc::Rc};
use winsafe::{self as w, co, guard::DeleteObjectGuard, gui, prelude::*};

/// Width of the column the artwork gets beside the controls, in logical pixels.
const ART_WIDTH: i32 = 240;
/// Gap between the character and the edges of the page, in logical pixels.
const ART_MARGIN: i32 = 8;
/// The ornament's opacity out of 255: about 65%.
const DECORATION_OPACITY: u8 = 166;
/// Corner radius of panels and buttons, in logical pixels.
const CORNER: i32 = 4;

/// A theme's colours.
pub(super) struct Palette {
    background: u32,
    panel: u32,
    border: u32,
    accent: u32,
    text: u32,
    muted: u32,
    status: u32,
    on_accent: u32,
    secondary_button: u32,
}

const EMBER: Palette = Palette {
    background: 0x00FB_FAF4,
    panel: 0x00FF_FEFA,
    border: 0x00C5_D1BC,
    accent: 0x0031_5D45,
    text: 0x001F_2A23,
    muted: 0x0065_7367,
    status: 0x0023_821C,
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
    on_accent: 0x00FF_FFFF,
    secondary_button: 0x00ED_F3F9,
};

/// A colour from `0xRRGGBB`.
const fn colour(hex: u32) -> w::COLORREF {
    let [_, red, green, blue] = hex.to_be_bytes();
    w::COLORREF::from_rgb(red, green, blue)
}

/// `from` moved `part` out of 255 of the way to `to`, channel by channel.
fn mix(from: u32, to: u32, part: u32) -> u32 {
    let channel = |shift: u32| {
        let (a, b) = ((from >> shift) & 0xFF, (to >> shift) & 0xFF);
        (a * (255 - part) + b * part + 127) / 255
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// The artwork of a theme: a standing character and a faint ornament, both 1024 by 1536 with
/// transparent surroundings.
struct Artwork {
    character: &'static [u8],
    decoration: &'static [u8],
}

const fn artwork(theme: Theme) -> Option<Artwork> {
    match theme {
        Theme::Classic => None,
        Theme::Ember => Some(Artwork {
            character: include_bytes!("../../assets/themes/ember/ember-character.png"),
            decoration: include_bytes!("../../assets/themes/ember/ember-decoration.png"),
        }),
        Theme::Selene => Some(Artwork {
            character: include_bytes!("../../assets/themes/selene/selene-character.png"),
            decoration: include_bytes!("../../assets/themes/selene/selene-decoration.png"),
        }),
        Theme::Nova => Some(Artwork {
            character: include_bytes!("../../assets/themes/nova/nova-character.png"),
            decoration: include_bytes!("../../assets/themes/nova/nova-decoration.png"),
        }),
    }
}

fn palette(theme: Theme) -> Option<&'static Palette> {
    match theme {
        Theme::Classic => None,
        Theme::Ember => Some(&EMBER),
        Theme::Selene => Some(&SELENE),
        Theme::Nova => Some(&NOVA),
    }
}

/// What a window's background is, which decides the colour of the labels sitting on it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Surface {
    /// The dialog itself: the window and the rows under the tab.
    Dialog,
    /// A tab page, whose labels sit on the group panels.
    Page,
}

impl Surface {
    /// The system colour classic draws this surface in.
    pub(super) const fn classic(self) -> co::COLOR {
        match self {
            Self::Dialog => co::COLOR::BTNFACE,
            Self::Page => co::COLOR::WINDOW,
        }
    }
}

/// The look currently chosen, shared by every handler that paints.
#[derive(Clone)]
pub(super) struct Skin(Rc<Inner>);

struct Inner {
    theme: Cell<Theme>,
    resources: RefCell<Resources>,
}

/// What painting needs besides the palette, made when first wanted and dropped when the theme
/// changes.
#[derive(Default)]
struct Resources {
    brushes: Option<Brushes>,
    /// The artwork at full size, decoded once.
    pictures: Option<Pictures>,
    /// The artwork scaled to the page it is drawn on.
    scaled: Option<Scaled>,
    title_font: Option<DeleteObjectGuard<w::HFONT>>,
}

struct Brushes {
    background: DeleteObjectGuard<w::HBRUSH>,
    panel: DeleteObjectGuard<w::HBRUSH>,
}

struct Pictures {
    character: Image,
    decoration: Image,
}

struct Scaled {
    /// The page height these were made for, in pixels.
    page_height: i32,
    character: Layer,
    decoration: Layer,
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

    /// Switches look. The next paint builds what the new one needs.
    pub(super) fn set(&self, theme: Theme) {
        self.0.theme.set(theme);
        let mut resources = self.0.resources.borrow_mut();
        resources.brushes = None;
        resources.pictures = None;
        resources.scaled = None;
    }

    pub(super) fn palette(&self) -> Option<&'static Palette> {
        palette(self.theme())
    }

    pub(super) fn is_themed(&self) -> bool {
        self.palette().is_some()
    }

    /// The width the window gains for the artwork, in logical pixels.
    pub(super) fn extra_width(&self) -> i32 {
        if self.is_themed() { ART_WIDTH } else { 0 }
    }

    /// The colour a label draws its text in: the classic colour, or its counterpart in the
    /// theme when it is one of the few the app uses.
    pub(super) fn text_colour(&self, classic: w::COLORREF) -> w::COLORREF {
        let Some(palette) = self.palette() else {
            return classic;
        };
        let known = |hex: u32| classic == colour_of_classic(hex);
        if known(CLASSIC_BLACK) {
            colour(palette.text)
        } else if known(CLASSIC_GRAY) {
            colour(palette.muted)
        } else if known(CLASSIC_GREEN) {
            colour(palette.status)
        } else {
            classic
        }
    }

    /// The brush labels erase themselves with, so that they blend into what is behind them.
    pub(super) fn label_brush(&self, surface: Surface) -> w::HBRUSH {
        if self.is_themed() {
            let mut resources = self.0.resources.borrow_mut();
            if let Some(brushes) = resources.ensure_brushes(self.palette()) {
                let brush = match surface {
                    Surface::Dialog => &brushes.background,
                    Surface::Page => &brushes.panel,
                };
                return copy_handle(&**brush);
            }
        }
        w::HBRUSH::GetSysColorBrush(surface.classic()).unwrap_or(w::HBRUSH::NULL)
    }

    /// Fills `hdc`'s whole window with the theme's background.
    pub(super) fn fill_dialog(&self, hdc: &w::HDC, window: &w::HWND) {
        let Ok(client) = window.GetClientRect() else {
            return;
        };
        let mut resources = self.0.resources.borrow_mut();
        if let Some(brushes) = resources.ensure_brushes(self.palette()) {
            let _ = hdc.FillRect(client, &brushes.background);
        }
    }

    /// Paints a tab page's background: the flat colour, the ornament, then the group panels.
    pub(super) fn paint_backdrop(&self, hdc: &w::HDC, page: &w::HWND, frames: &[Frame]) {
        let Some(palette) = self.palette() else {
            return;
        };
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        let theme = self.theme();
        let mut resources = self.0.resources.borrow_mut();
        resources.ensure_title_font(page);
        resources.ensure_scaled(theme, client.bottom - client.top);
        let Some(brushes) = resources.ensure_brushes(Some(palette)) else {
            return;
        };
        let _ = hdc.FillRect(client, &brushes.background);
        if let Some(scaled) = &resources.scaled {
            let (width, height) = scaled.decoration.size();
            scaled.decoration.draw(
                hdc,
                client.right - width,
                client.bottom - height,
                DECORATION_OPACITY,
            );
        }
        let Some(brushes) = &resources.brushes else {
            return;
        };
        let font = resources.title_font.as_deref();
        for frame in frames {
            paint_frame(hdc, palette, &brushes.panel, font, frame);
        }
    }

    /// Paints a whole tab page, backdrop and character, off-screen and then onto the window in
    /// one go. Painting the two in separate steps shows the page without the character in
    /// between.
    pub(super) fn paint_page(&self, hdc: &w::HDC, page: &w::HWND, frames: &[Frame]) {
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        buffered(hdc, (client.right, client.bottom), None, |target| {
            self.paint_backdrop(target, page, frames);
            self.paint_character(target, page);
        });
    }

    /// Paints the character over a tab page, bottom right.
    pub(super) fn paint_character(&self, hdc: &w::HDC, page: &w::HWND) {
        if !self.is_themed() {
            return;
        }
        let Ok(client) = page.GetClientRect() else {
            return;
        };
        let theme = self.theme();
        let mut resources = self.0.resources.borrow_mut();
        resources.ensure_scaled(theme, client.bottom - client.top);
        if let Some(scaled) = &resources.scaled {
            let (width, height) = scaled.character.size();
            let margin = gui::dpi_x(ART_MARGIN);
            scaled.character.draw(
                hdc,
                client.right - width - margin,
                client.bottom - height - margin,
                u8::MAX,
            );
        }
    }

    /// Draws a push button in the theme during `NM_CUSTOMDRAW`, or lets Windows draw it.
    /// `primary` buttons take the accent colour.
    pub(super) fn paint_button(
        &self,
        draw: &w::NMCUSTOMDRAW,
        text: &str,
        primary: bool,
    ) -> co::CDRF {
        let Some(palette) = self.palette() else {
            return co::CDRF::DODEFAULT;
        };
        if draw.dwDrawStage != co::CDDS::PREPAINT {
            return co::CDRF::DODEFAULT;
        }
        let state = draw.uItemState;
        let disabled = state.has(co::CDIS::DISABLED);
        let pressed = state.has(co::CDIS::SELECTED);
        let hot = state.has(co::CDIS::HOT);
        let (fill, edge, ink) = if primary && !disabled {
            let fill = if pressed {
                mix(palette.accent, 0, 40)
            } else if hot {
                mix(palette.accent, palette.on_accent, 28)
            } else {
                palette.accent
            };
            (fill, palette.accent, palette.on_accent)
        } else {
            let fill = if disabled {
                palette.secondary_button
            } else if pressed {
                mix(palette.secondary_button, palette.border, 150)
            } else if hot {
                palette.panel
            } else {
                palette.secondary_button
            };
            let edge = if hot && !disabled {
                palette.accent
            } else {
                palette.border
            };
            let ink = if disabled {
                palette.muted
            } else {
                palette.text
            };
            (fill, edge, ink)
        };

        let hdc = &draw.hdc;
        let (Ok(brush), Ok(pen)) = (
            w::HBRUSH::CreateSolidBrush(colour(fill)),
            w::HPEN::CreatePen(co::PS::SOLID, 1, colour(edge)),
        ) else {
            return co::CDRF::DODEFAULT;
        };
        // The corners show what is behind the button, not the button's own background.
        let _ = hdc.FillRect(draw.rc, &brush_of(&self.0.resources, palette, false));
        {
            let (Ok(_brush), Ok(_pen)) = (hdc.SelectObject(&*brush), hdc.SelectObject(&*pen))
            else {
                return co::CDRF::DODEFAULT;
            };
            let corner = gui::dpi_x(CORNER) * 2;
            let _ = hdc.RoundRect(
                w::RECT {
                    left: draw.rc.left,
                    top: draw.rc.top,
                    right: draw.rc.right,
                    bottom: draw.rc.bottom,
                },
                w::SIZE::with(corner, corner),
            );
        }
        let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);
        let _ = hdc.SetTextColor(colour(ink));
        let mut bounds = draw.rc;
        let _ = hdc.DrawText(
            text,
            &mut bounds,
            co::DT::CENTER | co::DT::VCENTER | co::DT::SINGLELINE | co::DT::NOPREFIX,
        );
        if state.has(co::CDIS::FOCUS) && state.has(co::CDIS::SHOWKEYBOARDCUES) {
            let inset = gui::dpi_x(3);
            let _ = hdc.DrawFocusRect(w::RECT {
                left: draw.rc.left + inset,
                top: draw.rc.top + inset,
                right: draw.rc.right - inset,
                bottom: draw.rc.bottom - inset,
            });
        }
        co::CDRF::SKIPDEFAULT
    }

    /// Paints the tab control: the strip with the tabs, and the frame round the pages.
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
        // The pages sit over the tab control's display area and paint it themselves, so the
        // tab control must not put anything there.
        let pages = display_rect(hwnd, client);
        buffered(&paint, (client.right, client.bottom), pages, |target| {
            draw_tabs(target, tab, palette, client);
        });
    }
}

// Classic colours the labels are given, which the theme answers with its own.
const CLASSIC_BLACK: u32 = 0x0000_0000;
const CLASSIC_GRAY: u32 = 0x006D_6D6D;
const CLASSIC_GREEN: u32 = 0x0010_7C10;

const fn colour_of_classic(hex: u32) -> w::COLORREF {
    colour(hex)
}

impl Resources {
    fn ensure_brushes(&mut self, palette: Option<&Palette>) -> Option<&Brushes> {
        if self.brushes.is_none() {
            let palette = palette?;
            self.brushes = Some(Brushes {
                background: w::HBRUSH::CreateSolidBrush(colour(palette.background)).ok()?,
                panel: w::HBRUSH::CreateSolidBrush(colour(palette.panel)).ok()?,
            });
        }
        self.brushes.as_ref()
    }

    /// The group titles use the dialog's font, a little larger and heavier.
    fn ensure_title_font(&mut self, window: &w::HWND) {
        if self.title_font.is_some() {
            return;
        }
        let Some(base) = control_font(window).and_then(|font| font.GetObject().ok()) else {
            return;
        };
        let mut font = base;
        font.lfHeight = font.lfHeight * 118 / 100;
        font.lfWeight = co::FW::SEMIBOLD;
        self.title_font = w::HFONT::CreateFontIndirect(&font).ok();
    }

    /// Scales the artwork to a page `page_height` pixels tall, unless it already is.
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
            let Some(artwork) = artwork(theme) else {
                return;
            };
            let (Some(character), Some(decoration)) = (
                Image::decode(artwork.character),
                Image::decode(artwork.decoration),
            ) else {
                return;
            };
            self.pictures = Some(Pictures {
                character,
                decoration,
            });
        }
        let Some(pictures) = &self.pictures else {
            return;
        };
        let margin = gui::dpi_y(ART_MARGIN);
        let character_height = usize::try_from(page_height - 2 * margin).unwrap_or(1);
        let decoration_height = usize::try_from(page_height).unwrap_or(1);
        let layers = (
            Layer::new(&pictures.character.scaled_to_height(character_height)),
            Layer::new(&pictures.decoration.scaled_to_height(decoration_height)),
        );
        self.scaled = match layers {
            (Some(character), Some(decoration)) => Some(Scaled {
                page_height,
                character,
                decoration,
            }),
            _ => None,
        };
    }
}

/// A brush of the palette's background or panel colour, for painting a button's corners.
fn brush_of(resources: &RefCell<Resources>, palette: &Palette, panel: bool) -> w::HBRUSH {
    let mut resources = resources.borrow_mut();
    resources
        .ensure_brushes(Some(palette))
        .map_or(w::HBRUSH::NULL, |brushes| {
            copy_handle(&**if panel {
                &brushes.panel
            } else {
                &brushes.background
            })
        })
}

/// A group box: a panel with a hairline border and its title sitting on the border.
fn paint_frame(
    hdc: &w::HDC,
    palette: &Palette,
    panel: &w::HBRUSH,
    title_font: Option<&w::HFONT>,
    frame: &Frame,
) {
    let rect = frame.rect();
    let Ok(pen) = w::HPEN::CreatePen(co::PS::SOLID, 1, colour(palette.border)) else {
        return;
    };
    {
        let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(&*pen), hdc.SelectObject(panel)) else {
            return;
        };
        let corner = gui::dpi_x(CORNER) * 2;
        let _ = hdc.RoundRect(rect, w::SIZE::with(corner, corner));
    }

    let _font = title_font.and_then(|font| hdc.SelectObject(font).ok());
    let title = frame.title();
    let Ok(size) = hdc.GetTextExtentPoint32(title) else {
        return;
    };
    let left = rect.left + gui::dpi_x(12);
    let top = rect.top - size.cy / 2;
    // The border breaks where the title sits.
    let _ = hdc.FillRect(
        w::RECT {
            left: left - gui::dpi_x(4),
            top: rect.top - 1,
            right: left + size.cx + gui::dpi_x(4),
            bottom: rect.top + 2,
        },
        panel,
    );
    let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);
    let _ = hdc.SetTextColor(colour(palette.accent));
    let mut bounds = w::RECT {
        left,
        top,
        right: left + size.cx + 2,
        bottom: top + size.cy,
    };
    let _ = hdc.DrawText(
        title,
        &mut bounds,
        co::DT::LEFT | co::DT::SINGLELINE | co::DT::NOPREFIX,
    );
}

/// The window's own title bar and border in the theme's colours, or the classic dialog's.
pub(super) fn title_bar_colours(skin: &Skin) -> Option<(w::COLORREF, w::COLORREF, w::COLORREF)> {
    skin.palette().map(|palette| {
        (
            colour(palette.background),
            colour(palette.text),
            colour(palette.border),
        )
    })
}

/// Draws the strip of tabs and the frame round the pages onto `paint`.
fn draw_tabs(paint: &w::HDC, tab: &gui::Tab, palette: &Palette, client: w::RECT) {
    let hwnd = tab.hwnd();
    let count = tab.items().count().unwrap_or(0);
    let selected = selected_tab(hwnd);
    let strip_bottom = (0..count)
        .filter_map(|index| tab_rect(hwnd, index))
        .map(|rect| rect.bottom)
        .max()
        .unwrap_or(0);

    let Ok(pen) = w::HPEN::CreatePen(co::PS::SOLID, 1, colour(palette.border)) else {
        return;
    };
    let Ok(background) = w::HBRUSH::CreateSolidBrush(colour(palette.background)) else {
        return;
    };
    let _ = paint.FillRect(client, &background);

    let font = control_font(hwnd);
    let _font = font.as_ref().and_then(|font| paint.SelectObject(font).ok());
    let _ = paint.SetBkMode(co::BKMODE::TRANSPARENT);

    // The frame round the pages, which the pages cover but for this border.
    {
        let (Ok(_pen), Ok(_brush)) = (
            paint.SelectObject(&*pen),
            paint.SelectObject(
                &w::HBRUSH::GetStockObject(co::STOCK_BRUSH::NULL).unwrap_or(w::HBRUSH::NULL),
            ),
        ) else {
            return;
        };
        let _ = paint.Rectangle(w::RECT {
            left: client.left,
            top: strip_bottom - 1,
            right: client.right,
            bottom: client.bottom,
        });
    }

    // The pen stays selected for every tab's outline.
    let _pen = paint.SelectObject(&*pen);
    for index in 0..count {
        let Some(rect) = tab_rect(hwnd, index) else {
            continue;
        };
        let is_selected = selected == Some(index);
        let fill = if is_selected {
            palette.panel
        } else {
            palette.secondary_button
        };
        let Ok(brush) = w::HBRUSH::CreateSolidBrush(colour(fill)) else {
            continue;
        };
        let _ = paint.FillRect(rect, &brush);
        // Top and sides; the selected tab opens into its page below.
        let _ = paint.MoveToEx(rect.left, rect.bottom, None);
        let _ = paint.LineTo(rect.left, rect.top);
        let _ = paint.LineTo(rect.right - 1, rect.top);
        let _ = paint.LineTo(rect.right - 1, rect.bottom);
        if is_selected {
            let _ = paint.FillRect(
                w::RECT {
                    left: rect.left + 1,
                    top: rect.bottom - 1,
                    right: rect.right - 1,
                    bottom: rect.bottom + 1,
                },
                &brush,
            );
        } else {
            let _ = paint.MoveToEx(rect.left, rect.bottom - 1, None);
            let _ = paint.LineTo(rect.right, rect.bottom - 1);
        }
        let text = tab
            .items()
            .iter()
            .ok()
            .and_then(|mut items| items.nth(index as usize))
            .and_then(|item| item.text().ok())
            .unwrap_or_default();
        let ink = if is_selected {
            palette.accent
        } else {
            palette.text
        };
        let _ = paint.SetTextColor(colour(ink));
        let mut bounds = rect;
        let _ = paint.DrawText(
            &text,
            &mut bounds,
            co::DT::CENTER | co::DT::VCENTER | co::DT::SINGLELINE | co::DT::NOPREFIX,
        );
    }
}

/// Runs `draw` on an off-screen surface `size` pixels across, then copies the result over
/// `hdc` in one go, so the screen never shows a half-painted state. Nothing is copied into
/// `keep_out`, which something else owns. Draws straight onto `hdc` when no off-screen surface
/// can be made.
fn buffered(
    hdc: &w::HDC,
    (width, height): (i32, i32),
    keep_out: Option<w::RECT>,
    draw: impl FnOnce(&w::HDC),
) {
    if width > 0
        && height > 0
        && let Ok(memory) = hdc.CreateCompatibleDC()
        && let Ok(bitmap) = hdc.CreateCompatibleBitmap(width, height)
        && let Ok(_selected) = memory.SelectObject(&*bitmap)
    {
        draw(&memory);
        if let Some(rect) = keep_out {
            let _ = hdc.ExcludeClipRect(rect);
        }
        let _ = hdc.BitBlt(
            w::POINT::default(),
            w::SIZE::with(width, height),
            &memory,
            w::POINT::default(),
            co::ROP::SRCCOPY,
        );
    } else {
        draw(hdc);
    }
}

/// A copy of a handle that something else owns, for a call that wants the handle by value.
#[expect(unsafe_code)]
fn copy_handle<H: Handle>(handle: &H) -> H {
    // SAFETY: the copy is only handed straight back to Windows for the duration of a paint,
    // while the owner (the skin's resources) is alive and unchanged.
    unsafe { handle.raw_copy() }
}

/// The font a control draws with, which stays the control's own.
#[expect(unsafe_code)]
fn control_font(hwnd: &w::HWND) -> Option<w::HFONT> {
    // SAFETY: WM_GETFONT takes no parameters and returns the control's font handle.
    unsafe { hwnd.SendMessage(w::msg::WmGetFont {}) }
}

/// The rectangle of tab `index`, in the tab control's client coordinates.
#[expect(unsafe_code)]
fn tab_rect(tab: &w::HWND, index: u32) -> Option<w::RECT> {
    let mut rect = w::RECT::default();
    // SAFETY: TCM_GETITEMRECT writes one rectangle, which `rect` provides.
    let found = unsafe {
        tab.SendMessage(w::msg::TcmGetItemRect {
            index,
            rect: &mut rect,
        })
    };
    found.ok().map(|()| rect)
}

/// The part of the tab control, in its client coordinates, that its pages occupy.
#[expect(unsafe_code)]
fn display_rect(tab: &w::HWND, client: w::RECT) -> Option<w::RECT> {
    let mut rect = client;
    // SAFETY: TCM_ADJUSTRECT turns the window rectangle in `rect` into the display rectangle.
    unsafe {
        tab.SendMessage(w::msg::TcmAdjustRect {
            display_rect: false,
            rect: &mut rect,
        });
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

/// The index of the selected tab.
#[expect(unsafe_code)]
fn selected_tab(tab: &w::HWND) -> Option<u32> {
    // SAFETY: TCM_GETCURSEL takes no parameters.
    unsafe { tab.SendMessage(w::msg::TcmGetCurSel {}) }
}

/// Lets the tab control paint itself, as when no theme is chosen.
#[expect(unsafe_code)]
pub(super) fn paint_tabs_natively(tab: &w::HWND) {
    // SAFETY: passes WM_PAINT on to the control's own window procedure, which `BeginPaint`s.
    unsafe {
        tab.DefSubclassProc(w::msg::Wm::new(co::WM::PAINT, 0, 0));
    }
}
