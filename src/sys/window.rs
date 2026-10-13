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

/// Shows the embedded icon `resource` in the title bar and the taskbar.
pub(crate) fn set_icon(window: &w::HWND, resource: u16) {
    let Ok(instance) = w::HINSTANCE::GetModuleHandle(None) else {
        return;
    };
    for (size, metric) in [
        (co::ICON_SZ::SMALL, co::SM::CXSMICON),
        (co::ICON_SZ::BIG, co::SM::CXICON),
    ] {
        let pixels = w::GetSystemMetrics(metric);
        let Ok(mut icon) = instance.LoadImageIcon(
            w::IdOicStr::Id(resource),
            w::SIZE::with(pixels, pixels),
            co::LR::DEFAULTCOLOR | co::LR::SHARED,
        ) else {
            continue;
        };
        // A shared resource icon is released by the system, not by `DestroyIcon`.
        let hicon = icon.leak();
        // SAFETY: `WM_SETICON` takes an icon handle the system owns.
        unsafe { window.SendMessage(w::msg::WmSetIcon { hicon, size }) };
    }
}
