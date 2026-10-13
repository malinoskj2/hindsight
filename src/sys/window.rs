//! Native window chrome and the raw messages the gui reads controls with: a flat
//! dialog-coloured title bar instead of Windows 11's backdrop, fonts and tab rectangles.

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

/// Whether a high-contrast theme is on, whose colours must replace any tint of the app's own.
pub(crate) fn high_contrast() -> bool {
    use windows::Win32::UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        WindowsAndMessaging::{
            SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        },
    };

    let Ok(size) = u32::try_from(size_of::<HIGHCONTRASTW>()) else {
        return false;
    };
    let mut contrast = HIGHCONTRASTW {
        cbSize: size,
        ..Default::default()
    };
    // SAFETY: SPI_GETHIGHCONTRAST fills the `size`-byte structure `contrast` for the call only.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            size,
            Some((&raw mut contrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_ok() && contrast.dwFlags.contains(HCF_HIGHCONTRASTON)
}

/// The font a control draws its text with; `None` for the system font.
pub(crate) fn control_font(hwnd: &w::HWND) -> Option<w::HFONT> {
    // SAFETY: WM_GETFONT takes no parameters.
    unsafe { hwnd.SendMessage(w::msg::WmGetFont {}) }
}

/// Where tab `index` of a tab control sits, in its client coordinates.
pub(crate) fn tab_rect(tab: &w::HWND, index: u32) -> Option<w::RECT> {
    let mut rect = w::RECT::default();
    // SAFETY: TCM_GETITEMRECT writes one rectangle into `rect`.
    let found = unsafe {
        tab.SendMessage(w::msg::TcmGetItemRect {
            index,
            rect: &mut rect,
        })
    };
    found.ok().map(|()| rect)
}

/// The part of a tab control's `client` area its pages cover; `None` when there is none.
pub(crate) fn display_rect(tab: &w::HWND, client: w::RECT) -> Option<w::RECT> {
    let mut rect = client;
    // SAFETY: TCM_ADJUSTRECT writes the display rectangle into `rect`.
    unsafe {
        tab.SendMessage(w::msg::TcmAdjustRect {
            display_rect: false,
            rect: &mut rect,
        });
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}
