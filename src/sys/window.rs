//! Native window chrome: a flat dialog-coloured title bar instead of Windows 11's
//! wallpaper-tinted backdrop, matching classic control-panel applets.

use winsafe::{self as w, co};

/// The caption colour of an ordinary light-theme dialog.
const CAPTION: w::COLORREF = w::COLORREF::from_rgb(0xF3, 0xF3, 0xF3);
const CAPTION_TEXT: w::COLORREF = w::COLORREF::from_rgb(0x00, 0x00, 0x00);

/// Applies the dialog title bar to the window.
pub(crate) fn match_dialog_title_bar(window: &w::HWND) {
    paint_title_bar(window, CAPTION, CAPTION_TEXT);
}

/// Applies a title bar of the given colours, for a window dressed in a theme.
pub(crate) fn paint_title_bar(window: &w::HWND, caption: w::COLORREF, text: w::COLORREF) {
    // Older Windows versions lack these attributes; the default chrome is then kept.
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::SystemBackdropType(co::DWMSBT::NONE));
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::CaptionColor(caption));
    let _ = window.DwmSetWindowAttribute(w::DwmAttr::TextColor(text));
}
