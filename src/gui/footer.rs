//! The row under the tab, added only when it has something to say: the hint that, without a
//! notification-area icon, closing the window quits.

use super::{
    TAB_HEIGHT,
    layout::{self, MARGIN, WINDOW_WIDTH, label},
};
use winsafe::gui;

/// Height of the row, in logical pixels.
pub(super) const HEIGHT: i32 = 30;

#[derive(Clone)]
pub(super) struct Footer {
    hint: gui::Label,
}

impl Footer {
    pub(super) fn new(wnd: &gui::WindowMain) -> Self {
        let y = MARGIN + TAB_HEIGHT + 6;
        Self {
            hint: label(wnd, "", MARGIN + 2, y, WINDOW_WIDTH - 2 * MARGIN, 1),
        }
    }

    /// The row's one control, which moves when the tab grows and is drawn muted.
    pub(super) fn control(&self) -> &gui::Label {
        &self.hint
    }

    /// Hides the row until it has something to say; only possible once the controls exist.
    pub(super) fn on_create(&self) {
        layout::show(&self.hint, false);
    }

    /// Shows the row with `message` in it.
    pub(super) fn show_hint(&self, message: &str) {
        layout::set_text(&self.hint, message);
        layout::show(&self.hint, true);
    }
}
