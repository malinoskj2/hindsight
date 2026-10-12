//! The row under the tab, shown only when there is a hint to give.

use super::{
    TAB_HEIGHT,
    layout::{self, MARGIN, WINDOW_WIDTH, label},
};
use winsafe::gui;

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

    pub(super) fn control(&self) -> &gui::Label {
        &self.hint
    }

    pub(super) fn on_create(&self) {
        layout::show(&self.hint, false);
    }

    pub(super) fn show_hint(&self, message: &str) {
        layout::set_text(&self.hint, message);
        layout::show(&self.hint, true);
    }
}
