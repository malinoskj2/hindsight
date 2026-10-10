//! The row under the tab, added only when it has something to say: the hint that, without a
//! notification-area icon, closing the window quits.

use super::{
    TAB_HEIGHT,
    layout::{self, BLACK, GRAY, MARGIN, WINDOW_WIDTH, label, set_text, show},
    theme::{Skin, Surface},
};
use winsafe::{gui, prelude::*};

/// Height of the row, in logical pixels.
pub(super) const HEIGHT: i32 = 30;

#[derive(Clone)]
pub(super) struct Footer {
    hint: gui::Label,
}

impl Footer {
    pub(super) fn new(wnd: &gui::WindowMain, skin: &Skin) -> Self {
        let y = MARGIN + TAB_HEIGHT + 6;
        let footer = Self {
            hint: label(wnd, "", MARGIN + 2, y, WINDOW_WIDTH - 2 * MARGIN, 1),
        };
        // The hint is muted; the window has no other labels.
        let colours = footer.clone();
        layout::draw(
            wnd,
            wnd.on(),
            Vec::new(),
            skin,
            Surface::Dialog,
            move |hwnd| {
                if *hwnd == *colours.hint.hwnd() {
                    GRAY
                } else {
                    BLACK
                }
            },
        );
        footer
    }

    /// Hidden until the row is shown, and moved when the tab grows.
    pub(super) fn control(&self) -> &gui::Label {
        &self.hint
    }

    pub(super) fn show_hint(&self, message: &str) {
        set_text(&self.hint, message);
        show(&self.hint, true);
    }
}
