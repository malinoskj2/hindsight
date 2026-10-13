//! What every tab's page shares: the window drives them all through `Pane`.

use super::{
    layout::{self, Frame, Painted},
    theme::{Skin, Surface},
};
use winsafe::gui;

/// The top of a page's first group frame.
pub(super) const FIRST_FRAME: i32 = 6;
/// From a group frame's top to its first row, below the title.
pub(super) const FIRST_ROW: i32 = 20;

/// A tab's page, as the window sees it.
pub(super) trait Pane {
    /// The tab's caption.
    fn title(&self) -> &'static str;

    fn page(&self) -> &gui::TabPage;

    /// Runs once the window and its controls exist.
    fn on_create(&self) {}

    /// Follows the window growing by `dy` pixels for the pages' messages.
    fn grow(&self, _dy: i32) {}

    /// The group frames, whose titles the classic look draws as labels.
    fn frames(&self) -> &[Frame] {
        &[]
    }
}

/// Paints `page` in `skin`, with the frames and tinted labels `painted` lists.
pub(super) fn paint(page: &gui::TabPage, skin: &Skin, painted: impl Painted + Clone + 'static) {
    layout::draw(page, page.on(), skin, Surface::Page, painted);
}
