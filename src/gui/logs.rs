//! The Logs tab: a read-only text box that receives journal entries as they are recorded.

use super::{
    layout::{self, FILL_HEIGHT, FILL_Y, GROUP_WIDTH, GROUP_X},
    pane::{self, Pane},
    text,
    theme::Skin,
};
use crate::log;
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

/// Characters after which the view is cleared; the session log file keeps everything.
const VIEW_LIMIT: i32 = 200_000;

#[derive(Clone)]
pub(super) struct LogsPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    view: gui::Edit,
    sequence: Cell<u64>,
    need_scroll: Cell<bool>,
}

impl Pane for LogsPage {
    fn title(&self) -> &'static str {
        text::TAB_LOGS
    }

    fn page(&self) -> &gui::TabPage {
        &self.0.page
    }

    fn on_create(&self) {
        // A multi-line edit otherwise stops accepting text after 32 K characters.
        self.0.view.limit_text(None);
    }

    fn grow(&self, dy: i32) {
        layout::resize_height_by(self.0.view.hwnd(), dy);
    }
}

impl LogsPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = layout::page(parent);
        let view = gui::Edit::new(
            &page,
            gui::EditOpts {
                text: "",
                position: gui::dpi(GROUP_X, FILL_Y),
                width: gui::dpi_x(GROUP_WIDTH),
                height: gui::dpi_y(FILL_HEIGHT),
                control_style: co::ES::MULTILINE
                    | co::ES::READONLY
                    | co::ES::AUTOVSCROLL
                    | co::ES::NOHIDESEL,
                window_style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::VSCROLL,
                ..Default::default()
            },
        );
        pane::paint(&page, skin, ());

        Self(Rc::new(Inner {
            page,
            view,
            sequence: Cell::new(0),
            need_scroll: Cell::new(false),
        }))
    }

    pub(super) fn update(&self) {
        let entries = log::since(self.0.sequence.get());
        if let Some(latest) = entries.last() {
            self.0.sequence.set(latest.sequence);
            self.0.need_scroll.set(true);
            self.append(&entries);
        }
        if self.0.need_scroll.get() && self.0.view.hwnd().IsWindowVisible() {
            self.scroll_to_end();
            self.0.need_scroll.set(false);
        }
    }

    fn append(&self, entries: &[log::Entry]) {
        if entries.is_empty() {
            return;
        }
        let view = &self.0.view;
        let mut length = view.hwnd().GetWindowTextLength().unwrap_or(0);
        if length > VIEW_LIMIT {
            let _ = view.set_text("");
            length = 0;
        }
        let text: String = entries
            .iter()
            .map(|entry| entry.line().replace('\n', "\r\n") + "\r\n")
            .collect();
        view.set_selection(length, length);
        view.replace_selection(&text);
    }

    /// A read-only edit without focus does not follow its caret, and a hidden one ignores the
    /// scroll, so this runs once the tab is showing.
    #[expect(unsafe_code, reason = "sends EM_SCROLLCARET")]
    fn scroll_to_end(&self) {
        let view = &self.0.view;
        let length = view.hwnd().GetWindowTextLength().unwrap_or(0);
        view.set_selection(length, length);
        // SAFETY: EM_SCROLLCARET takes no parameters.
        unsafe { view.hwnd().SendMessage(w::msg::EmScrollCaret {}) };
    }
}
