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
    layout::{self, Frame},
};
use crate::config::Theme;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use winsafe::{self as w, co, guard::DeleteObjectGuard, gui, prelude::*};

/// Width of the column the artwork gets beside the controls, in logical pixels.
const ART_WIDTH: i32 = 240;
/// Gap between the character and the edges of the page, in logical pixels.
const ART_MARGIN: i32 = 8;
/// The ornament's opacity out of 255: about 65%.
const DECORATION_OPACITY: u8 = 166;
/// Corner radius of panels and buttons, in logical pixels.
const CORNER: i32 = 4;
/// How far a pressed primary button darkens its accent towards black, out of 255.
const PRESSED_PRIMARY: u32 = 40;
/// How far a hovered primary button lightens its accent towards its text, out of 255.
const HOT_PRIMARY: u32 = 28;
/// Gap between a button's edge and its keyboard-focus rectangle, in logical pixels.
const FOCUS_INSET: i32 = 3;
/// How far a painted group title sits from its panel's left edge, in logical pixels.
const TITLE_INDENT: i32 = 12;
/// The break in the border either side of a painted group title, in logical pixels.
const TITLE_GAP: i32 = 4;
/// The group titles' font size, as a percentage of the dialog font's.
const TITLE_FONT_PERCENT: i32 = 118;
/// Black, as `0xRRGGBB`.
const BLACK_RGB: u32 = 0x0000_0000;

/// A theme's colours, each as `0xRRGGBB`.
struct Palette {
    background: u32,
    panel: u32,
    border: u32,
    accent: u32,
    text: u32,
    muted: u32,
    status: u32,
    on_accent: u32,
    /// The quiet fill of a tab not selected and of a disabled button.
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

/// The colours of `theme`; `None` in classic.
const fn palette(theme: Theme) -> Option<&'static Palette> {
    match theme {
        Theme::Classic => None,
        Theme::Ember => Some(&EMBER),
        Theme::Selene => Some(&SELENE),
        Theme::Nova => Some(&NOVA),
    }
}

/// The artwork of a theme: a standing character and a faint ornament, both 1024 by 1536 with
/// transparent surroundings.
struct Artwork {
    character: &'static [u8],
    decoration: &'static [u8],
}

/// The artwork of `theme`; `None` in classic.
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

    /// Whether a character theme is chosen rather than classic.
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
        if classic == layout::BLACK {
            colour(palette.text)
        } else if classic == layout::GRAY {
            colour(palette.muted)
        } else if classic == layout::GREEN {
            colour(palette.status)
        } else {
            classic
        }
    }

    /// The window's own title bar in the theme's colours, caption then text, or `None` for
    /// the classic dialog's.
    pub(super) fn title_bar_colours(&self) -> Option<(w::COLORREF, w::COLORREF)> {
        self.palette()
            .map(|palette| (colour(palette.background), colour(palette.text)))
    }

    /// A list's background and text in the theme's colours, those of a group panel so that it
    /// sits on the page like one, or `None` for Windows' own.
    pub(super) fn list_colours(&self) -> Option<(w::COLORREF, w::COLORREF)> {
        self.palette()
            .map(|palette| (colour(palette.panel), colour(palette.text)))
    }

    /// The brush labels erase themselves with, so that they blend into what is behind them.
    pub(super) fn label_brush(&self, surface: Surface) -> w::HBRUSH {
        self.brush(surface).unwrap_or_else(|| {
            w::HBRUSH::GetSysColorBrush(surface.classic()).unwrap_or(w::HBRUSH::NULL)
        })
    }

    /// Fills `dialog`'s client area on `hdc` with the theme's background.
    pub(super) fn fill_dialog(&self, hdc: &w::HDC, dialog: &w::HWND) {
        let Ok(client) = dialog.GetClientRect() else {
            return;
        };
        if let Some(background) = self.brush(Surface::Dialog) {
            let _ = hdc.FillRect(client, &background);
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
        if resources.ensure_brushes(Some(palette)).is_none() {
            return;
        }
        let Resources {
            brushes: Some(brushes),
            scaled,
            title_font,
            ..
        } = &*resources
        else {
            return;
        };
        let _ = hdc.FillRect(client, &brushes.background);
        if let Some(scaled) = scaled {
            let (width, height) = scaled.decoration.size();
            scaled.decoration.draw(
                hdc,
                client.right - width,
                client.bottom - height,
                DECORATION_OPACITY,
            );
        }
        for frame in frames {
            paint_frame(hdc, palette, &brushes.panel, title_font.as_deref(), frame);
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
        let (Ok(brush), Ok(pen)) = (solid_brush(fill), solid_pen(edge)) else {
            return co::CDRF::DODEFAULT;
        };
        // The corners show what is behind the button: the group panel it sits on.
        let behind = self.brush(Surface::Page).unwrap_or(w::HBRUSH::NULL);
        let _ = hdc.FillRect(draw.rc, &behind);
        if !round_rect(hdc, draw.rc, &pen, &brush) {
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
            paint_tab_strip(target, palette, tab, client);
        });
    }

    /// The colours of the look chosen; `None` in classic.
    fn palette(&self) -> Option<&'static Palette> {
        palette(self.theme())
    }

    /// A copy of the theme's brush for `surface`, made if need be; `None` in classic.
    fn brush(&self, surface: Surface) -> Option<w::HBRUSH> {
        let mut resources = self.0.resources.borrow_mut();
        resources
            .ensure_brushes(self.palette())
            .map(|brushes| copy_handle(brushes.of(surface)))
    }
}

/// What painting needs besides the palette, made when first wanted. Everything drawn in the
/// theme's colours or artwork is dropped when the theme changes; the title font is not.
#[derive(Default)]
struct Resources {
    brushes: Option<Brushes>,
    /// The artwork at full size, decoded once.
    pictures: Option<Pictures>,
    /// The artwork scaled to the page it is drawn on.
    scaled: Option<Scaled>,
    title_font: Option<DeleteObjectGuard<w::HFONT>>,
}

impl Resources {
    /// The theme's brushes, made from `palette` if need be; `None` in classic.
    fn ensure_brushes(&mut self, palette: Option<&Palette>) -> Option<&Brushes> {
        if self.brushes.is_none() {
            let palette = palette?;
            self.brushes = Some(Brushes {
                background: solid_brush(palette.background).ok()?,
                panel: solid_brush(palette.panel).ok()?,
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
        font.lfHeight = font.lfHeight * TITLE_FONT_PERCENT / 100;
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
            self.pictures = Pictures::decode(theme);
        }
        let Some(pictures) = &self.pictures else {
            return;
        };
        self.scaled = Scaled::new(pictures, page_height);
    }
}

/// The brushes the theme fills its surfaces with.
struct Brushes {
    background: DeleteObjectGuard<w::HBRUSH>,
    panel: DeleteObjectGuard<w::HBRUSH>,
}

impl Brushes {
    /// The brush `surface` is filled with.
    fn of(&self, surface: Surface) -> &w::HBRUSH {
        match surface {
            Surface::Dialog => &self.background,
            Surface::Page => &self.panel,
        }
    }
}

/// The theme's artwork, decoded at full size.
struct Pictures {
    character: Image,
    decoration: Image,
}

impl Pictures {
    /// The artwork of `theme`, decoded; `None` in classic or when either picture is unreadable.
    fn decode(theme: Theme) -> Option<Self> {
        let artwork = artwork(theme)?;
        Some(Self {
            character: Image::decode(artwork.character)?,
            decoration: Image::decode(artwork.decoration)?,
        })
    }
}

/// The theme's artwork, scaled to a page and ready to blend.
struct Scaled {
    /// The page height these were made for, in pixels.
    page_height: i32,
    character: Layer,
    decoration: Layer,
}

impl Scaled {
    /// `pictures` scaled to a page `page_height` pixels tall: the character within the page's
    /// margins, the ornament its full height. `None` when either bitmap cannot be made.
    fn new(pictures: &Pictures, page_height: i32) -> Option<Self> {
        let margin = gui::dpi_y(ART_MARGIN);
        let character_height = usize::try_from(page_height - 2 * margin).unwrap_or(1);
        let decoration_height = usize::try_from(page_height).unwrap_or(1);
        let layers = (
            Layer::new(&pictures.character.scaled_to_height(character_height)),
            Layer::new(&pictures.decoration.scaled_to_height(decoration_height)),
        );
        match layers {
            (Some(character), Some(decoration)) => Some(Self {
                page_height,
                character,
                decoration,
            }),
            _ => None,
        }
    }
}

/// One tab of the strip, as it is drawn.
struct TabFace {
    /// In the tab control's client coordinates.
    rect: w::RECT,
    text: String,
    selected: bool,
}

/// Lets the tab control paint itself, as when no theme is chosen.
#[expect(unsafe_code)]
pub(super) fn paint_tabs_natively(tab: &w::HWND) {
    // SAFETY: passes WM_PAINT on to the control's own window procedure, which `BeginPaint`s.
    unsafe {
        tab.DefSubclassProc(w::msg::Wm::new(co::WM::PAINT, 0, 0));
    }
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
    let Ok(pen) = solid_pen(palette.border) else {
        return;
    };
    if !round_rect(hdc, rect, &pen, panel) {
        return;
    }
    let _font = title_font.and_then(|font| hdc.SelectObject(font).ok());
    paint_title(hdc, palette, panel, frame);
}

/// A group box's title in the font selected into `hdc`, on a break in the top border of its
/// `panel`-filled frame.
fn paint_title(hdc: &w::HDC, palette: &Palette, panel: &w::HBRUSH, frame: &Frame) {
    let rect = frame.rect();
    let title = frame.title();
    let Ok(size) = hdc.GetTextExtentPoint32(title) else {
        return;
    };
    let left = rect.left + gui::dpi_x(TITLE_INDENT);
    let top = rect.top - size.cy / 2;
    // The border breaks where the title sits.
    let gap = gui::dpi_x(TITLE_GAP);
    let _ = hdc.FillRect(
        w::RECT {
            left: left - gap,
            top: rect.top - 1,
            right: left + size.cx + gap,
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

/// Paints the strip of tabs and the frame round the pages onto `hdc`.
fn paint_tab_strip(hdc: &w::HDC, palette: &Palette, tab: &gui::Tab, client: w::RECT) {
    let faces = tab_faces(tab);
    let strip_bottom = faces.iter().map(|face| face.rect.bottom).max().unwrap_or(0);

    let Ok(pen) = solid_pen(palette.border) else {
        return;
    };
    let Ok(background) = solid_brush(palette.background) else {
        return;
    };
    let _ = hdc.FillRect(client, &background);

    let font = control_font(tab.hwnd());
    let _font = font.as_ref().and_then(|font| hdc.SelectObject(font).ok());
    let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);

    let frame = w::RECT {
        top: strip_bottom - 1,
        ..client
    };
    if !outline_rect(hdc, frame, &pen) {
        return;
    }

    // The pen stays selected for every tab's outline.
    let _pen = hdc.SelectObject(&*pen);
    for face in &faces {
        paint_tab(hdc, palette, face);
    }
}

/// The tabs of the strip in order, as they are drawn; one whose rectangle the control cannot
/// give is left out.
fn tab_faces(tab: &gui::Tab) -> Vec<TabFace> {
    let hwnd = tab.hwnd();
    let items = tab.items();
    let selected = items.selected().map(|item| item.index());
    (0..items.count().unwrap_or(0))
        .filter_map(|index| {
            Some(TabFace {
                rect: tab_rect(hwnd, index)?,
                text: items.get(index).text().unwrap_or_default(),
                selected: selected == Some(index),
            })
        })
        .collect()
}

/// Paints one tab with the pen already selected into `hdc`: its fill, its outline open towards
/// the page when it is the selected one, and its caption.
fn paint_tab(hdc: &w::HDC, palette: &Palette, face: &TabFace) {
    let TabFace {
        rect,
        text,
        selected,
    } = face;
    let fill = if *selected {
        palette.panel
    } else {
        palette.secondary_button
    };
    let Ok(brush) = solid_brush(fill) else {
        return;
    };
    let _ = hdc.FillRect(*rect, &brush);
    // Top and sides; the selected tab opens into its page below.
    let _ = hdc.MoveToEx(rect.left, rect.bottom, None);
    let _ = hdc.LineTo(rect.left, rect.top);
    let _ = hdc.LineTo(rect.right - 1, rect.top);
    let _ = hdc.LineTo(rect.right - 1, rect.bottom);
    if *selected {
        let _ = hdc.FillRect(
            w::RECT {
                left: rect.left + 1,
                top: rect.bottom - 1,
                right: rect.right - 1,
                bottom: rect.bottom + 1,
            },
            &brush,
        );
    } else {
        let _ = hdc.MoveToEx(rect.left, rect.bottom - 1, None);
        let _ = hdc.LineTo(rect.right, rect.bottom - 1);
    }
    let ink = if *selected {
        palette.accent
    } else {
        palette.text
    };
    let _ = hdc.SetTextColor(colour(ink));
    draw_centred(hdc, text, *rect);
}

/// A push button's fill, border and text colours, as `0xRRGGBB`, in `state`. Every button is
/// a page's main action and takes the accent colour; a disabled one goes quiet.
fn button_colours(palette: &Palette, state: co::CDIS) -> (u32, u32, u32) {
    if state.has(co::CDIS::DISABLED) {
        return (palette.secondary_button, palette.border, palette.muted);
    }
    let fill = if state.has(co::CDIS::SELECTED) {
        mix(palette.accent, BLACK_RGB, PRESSED_PRIMARY)
    } else if state.has(co::CDIS::HOT) {
        mix(palette.accent, palette.on_accent, HOT_PRIMARY)
    } else {
        palette.accent
    };
    (fill, palette.accent, palette.on_accent)
}

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

/// A brush of one colour, given as `0xRRGGBB`.
fn solid_brush(hex: u32) -> w::SysResult<DeleteObjectGuard<w::HBRUSH>> {
    w::HBRUSH::CreateSolidBrush(colour(hex))
}

/// A hairline pen of one colour, given as `0xRRGGBB`.
fn solid_pen(hex: u32) -> w::SysResult<DeleteObjectGuard<w::HPEN>> {
    w::HPEN::CreatePen(co::PS::SOLID, 1, colour(hex))
}

/// Draws `rect` with rounded corners, outlined with `pen` and filled with `brush`; reports
/// whether the two could be selected to draw it.
fn round_rect(hdc: &w::HDC, rect: w::RECT, pen: &w::HPEN, brush: &w::HBRUSH) -> bool {
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(pen), hdc.SelectObject(brush)) else {
        return false;
    };
    let corner = gui::dpi_x(CORNER) * 2;
    let _ = hdc.RoundRect(rect, w::SIZE::with(corner, corner));
    true
}

/// Outlines `rect` with `pen`, leaving its inside alone: the frame round a tab control's pages,
/// which the pages cover but for this border. Reports whether the pen and the hollow brush could
/// be selected to draw it.
fn outline_rect(hdc: &w::HDC, rect: w::RECT, pen: &w::HPEN) -> bool {
    let hollow = w::HBRUSH::GetStockObject(co::STOCK_BRUSH::NULL).unwrap_or(w::HBRUSH::NULL);
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(pen), hdc.SelectObject(&hollow)) else {
        return false;
    };
    let _ = hdc.Rectangle(rect);
    true
}

/// Draws `text` on one line, centred in `rect`.
fn draw_centred(hdc: &w::HDC, text: &str, mut rect: w::RECT) {
    let _ = hdc.DrawText(
        text,
        &mut rect,
        co::DT::CENTER | co::DT::VCENTER | co::DT::SINGLELINE | co::DT::NOPREFIX,
    );
}

/// Draws the keyboard-focus rectangle just inside a button's `rect`.
fn draw_focus(hdc: &w::HDC, rect: w::RECT) {
    let inset = gui::dpi_x(FOCUS_INSET);
    let _ = hdc.DrawFocusRect(w::RECT {
        left: rect.left + inset,
        top: rect.top + inset,
        right: rect.right - inset,
        bottom: rect.bottom - inset,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_moves_each_channel_part_of_the_way_and_rounds_to_nearest() {
        assert_eq!(mix(0x0010_2030, 0x00FF_FFFF, 0), 0x0010_2030);
        assert_eq!(mix(0x0010_2030, 0x00FF_FFFF, 255), 0x00FF_FFFF);
        // Halfway from black to white is 128 (127.5 rounded up) in every channel.
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
    fn a_disabled_button_goes_quiet_whatever_else_its_state_says() {
        let state = co::CDIS::DISABLED | co::CDIS::HOT | co::CDIS::SELECTED;
        assert_eq!(
            button_colours(&EMBER, state),
            (EMBER.secondary_button, EMBER.border, EMBER.muted)
        );
    }

    #[test]
    fn a_button_keeps_its_accent_edge_and_darkens_when_pressed() {
        assert_eq!(
            button_colours(&EMBER, co::CDIS::default()),
            (EMBER.accent, EMBER.accent, EMBER.on_accent)
        );
        assert_eq!(
            button_colours(&EMBER, co::CDIS::SELECTED | co::CDIS::HOT),
            (
                mix(EMBER.accent, BLACK_RGB, PRESSED_PRIMARY),
                EMBER.accent,
                EMBER.on_accent
            )
        );
        assert_eq!(
            button_colours(&EMBER, co::CDIS::HOT),
            (
                mix(EMBER.accent, EMBER.on_accent, HOT_PRIMARY),
                EMBER.accent,
                EMBER.on_accent
            )
        );
    }

    #[test]
    fn a_list_takes_the_panel_colours_in_a_theme_and_none_in_classic() {
        let raw = |colours: Option<(w::COLORREF, w::COLORREF)>| {
            colours.map(|(background, text)| (background.raw(), text.raw()))
        };
        assert_eq!(raw(Skin::new(Theme::Classic).list_colours()), None);
        assert_eq!(
            raw(Skin::new(Theme::Ember).list_colours()),
            Some((colour(EMBER.panel).raw(), colour(EMBER.text).raw()))
        );
    }
}
