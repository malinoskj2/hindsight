//! Native window chrome: a flat dialog-coloured title bar instead of Windows 11's backdrop.

use winsafe::{self as w, co};

const CAPTION: w::COLORREF = w::COLORREF::from_rgb(0xF3, 0xF3, 0xF3);
const CAPTION_TEXT: w::COLORREF = w::COLORREF::from_rgb(0x00, 0x00, 0x00);

pub(crate) fn match_dialog_title_bar(window: &w::HWND) {
    paint_title_bar(window, CAPTION, CAPTION_TEXT);
}

pub(crate) fn paint_title_bar(window: &w::HWND, caption: w::COLORREF, text: w::COLORREF) {
    // Older Windows versions lack these attributes; the default chrome is then kept.
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::SystemBackdropType(co::DWMSBT::NONE));
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::CaptionColor(caption));
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::TextColor(text));
}
