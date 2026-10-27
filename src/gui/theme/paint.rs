//! The themed painters: page frames, the tab strip and buttons, drawn with the cached brushes.

use super::{
    cache::Gdi,
    palette::{Edge, Fill, Palette, colour},
};
use crate::{gui::layout::Frame, sys::window};
use winsafe::{self as w, co, gui, prelude::*};

const CORNER: i32 = 4;
const FOCUS_INSET: i32 = 3;
const TITLE_INDENT: i32 = 12;
const TITLE_GAP: i32 = 4;
/// Half the long side of a disclosure's arrow.
const ARROW: i32 = 3;
const ARROW_CENTRE: i32 = 4;
const CAPTION_INDENT: i32 = 12;
const RULE_GAP: i32 = 6;
/// The height of the group box drawn to leave only its top edge as a rule, enough to keep its
/// lower corners outside the clip.
const RULE_FRAME_HEIGHT: i32 = 16;

/// What draws a disclosure's rule.
pub(super) enum Rule<'a> {
    /// The system's group box edge, drawn for the control, so the rule matches the native frames.
    Native(&'a w::HWND),
    Colour(w::COLORREF),
}

struct TabFace {
    rect: w::RECT,
    text: String,
    selected: bool,
}

#[expect(unsafe_code, reason = "calls DefSubclassProc")]
pub(in crate::gui) fn paint_tabs_natively(tab: &w::HWND) {
    // SAFETY: forwards WM_PAINT to the control's own window procedure.
    unsafe {
        tab.DefSubclassProc(w::msg::Wm::new(co::WM::PAINT, 0, 0));
    }
}

pub(super) fn paint_frame(
    hdc: &w::HDC,
    palette: &Palette,
    gdi: &Gdi,
    title_font: Option<&w::HFONT>,
    frame: &Frame,
) {
    let rect = frame.rect();
    let panel = gdi.brush(Fill::Panel);
    if !round_rect(hdc, rect, gdi.pen(Edge::Border), panel) {
        return;
    }
    let _font = title_font.and_then(|font| hdc.SelectObject(font).ok());
    paint_title(hdc, palette, panel, frame);
}

fn paint_title(hdc: &w::HDC, palette: &Palette, panel: &w::HBRUSH, frame: &Frame) {
    let rect = frame.rect();
    let title = frame.title();
    let Ok(size) = hdc.GetTextExtentPoint32(title) else {
        return;
    };
    let left = rect.left + gui::dpi_x(TITLE_INDENT);
    let top = rect.top - size.cy / 2;
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

pub(super) fn paint_tab_strip(
    hdc: &w::HDC,
    palette: &Palette,
    gdi: &Gdi,
    tab: &gui::Tab,
    client: w::RECT,
) {
    let faces = tab_faces(tab);
    let strip_bottom = faces.iter().map(|face| face.rect.bottom).max().unwrap_or(0);

    let pen = gdi.pen(Edge::Border);
    let _ = hdc.FillRect(client, gdi.brush(Fill::Background));

    let font = window::control_font(tab.hwnd());
    let _font = font.as_ref().and_then(|font| hdc.SelectObject(font).ok());
    let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);

    let frame = w::RECT {
        top: strip_bottom - 1,
        ..client
    };
    if !outline_rect(hdc, frame, pen) {
        return;
    }

    let _pen = hdc.SelectObject(pen);
    for face in &faces {
        paint_tab(hdc, palette, gdi, face);
    }
}

fn tab_faces(tab: &gui::Tab) -> Vec<TabFace> {
    let hwnd = tab.hwnd();
    let items = tab.items();
    let selected = items.selected().map(|item| item.index());
    (0..items.count().unwrap_or(0))
        .filter_map(|index| {
            Some(TabFace {
                rect: window::tab_rect(hwnd, index)?,
                text: items.get(index).text().unwrap_or_default(),
                selected: selected == Some(index),
            })
        })
        .collect()
}

fn paint_tab(hdc: &w::HDC, palette: &Palette, gdi: &Gdi, face: &TabFace) {
    let TabFace {
        rect,
        text,
        selected,
    } = face;
    let brush = gdi.brush(if *selected {
        Fill::Panel
    } else {
        Fill::SecondaryButton
    });
    let _ = hdc.FillRect(*rect, brush);
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
            brush,
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

/// The fill, edge and text colour of a button in `state`.
pub(super) fn button_colours(palette: &Palette, state: co::CDIS) -> (Fill, Edge, u32) {
    if state.has(co::CDIS::DISABLED) {
        return (Fill::SecondaryButton, Edge::Border, palette.muted);
    }
    let fill = if state.has(co::CDIS::SELECTED) {
        Fill::Pressed
    } else if state.has(co::CDIS::HOT) {
        Fill::Hot
    } else {
        Fill::Accent
    };
    (fill, Edge::Accent, palette.on_accent)
}

/// Draws a disclosure in `rect`: an arrow that points down while `open`, the caption, and a rule
/// from the caption out to the right edge. The background is left to the caller.
pub(super) fn paint_disclosure(
    hdc: &w::HDC,
    rect: w::RECT,
    text: &str,
    open: bool,
    ink: w::COLORREF,
    focused: bool,
    rule: &Rule,
) {
    let middle = rect.top.midpoint(rect.bottom);
    paint_arrow(hdc, rect.left + gui::dpi_x(ARROW_CENTRE), middle, open, ink);

    let caption_left = rect.left + gui::dpi_x(CAPTION_INDENT);
    let (caption_width, caption_height) = hdc
        .GetTextExtentPoint32(text)
        .map_or((0, 0), |size| (size.cx, size.cy));
    let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);
    let _ = hdc.SetTextColor(ink);
    let mut caption = w::RECT {
        left: caption_left,
        right: caption_left + caption_width,
        ..rect
    };
    let _ = hdc.DrawText(
        text,
        &mut caption,
        co::DT::LEFT | co::DT::VCENTER | co::DT::SINGLELINE | co::DT::NOPREFIX,
    );
    if focused {
        let inset = gui::dpi_x(2);
        let _ = hdc.DrawFocusRect(w::RECT {
            left: caption_left - inset,
            top: middle - caption_height / 2 - 1,
            right: caption_left + caption_width + inset,
            bottom: middle + caption_height / 2 + 1,
        });
    }

    let line = w::RECT {
        left: caption_left + caption_width + gui::dpi_x(RULE_GAP),
        top: middle,
        right: rect.right,
        bottom: middle + 1,
    };
    if line.left < line.right {
        paint_rule(hdc, line, rule);
    }
}

fn paint_arrow(hdc: &w::HDC, centre: i32, middle: i32, open: bool, ink: w::COLORREF) {
    let long = gui::dpi_x(ARROW);
    let short = long / 2;
    let points = if open {
        [
            w::POINT::with(centre - long, middle - short),
            w::POINT::with(centre + long, middle - short),
            w::POINT::with(centre, middle + long - short),
        ]
    } else {
        [
            w::POINT::with(centre - short, middle - long),
            w::POINT::with(centre + long - short, middle),
            w::POINT::with(centre - short, middle + long),
        ]
    };
    let (Ok(pen), Ok(brush)) = (
        w::HPEN::GetStockObject(co::STOCK_PEN::DC),
        w::HBRUSH::GetStockObject(co::STOCK_BRUSH::DC),
    ) else {
        return;
    };
    let _ = hdc.SetDCPenColor(ink);
    let _ = hdc.SetDCBrushColor(ink);
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(&pen), hdc.SelectObject(&brush)) else {
        return;
    };
    let _ = hdc.Polygon(&points);
}

fn paint_rule(hdc: &w::HDC, line: w::RECT, rule: &Rule) {
    match rule {
        Rule::Native(control) => {
            let Some(theme) = control.OpenThemeData("BUTTON") else {
                return;
            };
            // Clipped to the line, a group box whose top edge lies on it leaves only that edge.
            let reach = gui::dpi_x(CORNER) * 2;
            let frame = w::RECT {
                left: line.left - reach,
                right: line.right + reach,
                bottom: line.top + gui::dpi_y(RULE_FRAME_HEIGHT),
                ..line
            };
            let _ =
                theme.DrawThemeBackground(hdc, co::VS::BUTTON_GROUPBOX_NORMAL, frame, Some(line));
        }
        Rule::Colour(colour) => {
            let Ok(brush) = w::HBRUSH::GetStockObject(co::STOCK_BRUSH::DC) else {
                return;
            };
            let _ = hdc.SetDCBrushColor(*colour);
            let _ = hdc.FillRect(line, &brush);
        }
    }
}

pub(super) fn round_rect(hdc: &w::HDC, rect: w::RECT, pen: &w::HPEN, brush: &w::HBRUSH) -> bool {
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(pen), hdc.SelectObject(brush)) else {
        return false;
    };
    let corner = gui::dpi_x(CORNER) * 2;
    let _ = hdc.RoundRect(rect, w::SIZE::with(corner, corner));
    true
}

fn outline_rect(hdc: &w::HDC, rect: w::RECT, pen: &w::HPEN) -> bool {
    let hollow = w::HBRUSH::GetStockObject(co::STOCK_BRUSH::NULL).unwrap_or(w::HBRUSH::NULL);
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(pen), hdc.SelectObject(&hollow)) else {
        return false;
    };
    let _ = hdc.Rectangle(rect);
    true
}

pub(super) fn draw_centred(hdc: &w::HDC, text: &str, mut rect: w::RECT) {
    let _ = hdc.DrawText(
        text,
        &mut rect,
        co::DT::CENTER | co::DT::VCENTER | co::DT::SINGLELINE | co::DT::NOPREFIX,
    );
}

pub(super) fn draw_focus(hdc: &w::HDC, rect: w::RECT) {
    let inset = gui::dpi_x(FOCUS_INSET);
    let _ = hdc.DrawFocusRect(w::RECT {
        left: rect.left + inset,
        top: rect.top + inset,
        right: rect.right - inset,
        bottom: rect.bottom - inset,
    });
}

pub(super) fn buffered(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Theme, gui::theme::palette::palette};

    #[test]
    fn a_disabled_button_goes_quiet_whatever_else_its_state_says() {
        let ember = palette(Theme::Ember).unwrap();
        let state = co::CDIS::DISABLED | co::CDIS::HOT | co::CDIS::SELECTED;
        assert_eq!(
            button_colours(ember, state),
            (Fill::SecondaryButton, Edge::Border, ember.muted)
        );
    }

    #[test]
    fn a_button_keeps_its_accent_edge_and_darkens_when_pressed() {
        let ember = palette(Theme::Ember).unwrap();
        assert_eq!(
            button_colours(ember, co::CDIS::default()),
            (Fill::Accent, Edge::Accent, ember.on_accent)
        );
        assert_eq!(
            button_colours(ember, co::CDIS::SELECTED | co::CDIS::HOT),
            (Fill::Pressed, Edge::Accent, ember.on_accent)
        );
        assert_eq!(
            button_colours(ember, co::CDIS::HOT),
            (Fill::Hot, Edge::Accent, ember.on_accent)
        );
    }
}
