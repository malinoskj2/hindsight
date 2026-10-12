//! The Logs tab: a read-only text box that receives journal entries as they are recorded.

use super::{
    layout::{self, BLACK, FILL_HEIGHT, FILL_Y, GROUP_WIDTH, GROUP_X},
    theme::{Skin, Surface},
};
use crate::log;
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

/// Characters after which the view is cleared; the session log file keeps everything.
const VIEW_LIMIT: i32 = 200_000;

#[derive(Clone)]
pub(super) struct LogsPage {
    page: gui::TabPage,
    view: gui::Edit,
    tail: Rc<Tail>,
}

impl LogsPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
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
        layout::draw(&page, page.on(), Vec::new(), skin, Surface::Page, |_| BLACK);

        Self {
            page,
            view,
            tail: Rc::default(),
        }
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    pub(super) fn view(&self) -> &gui::Edit {
        &self.view
    }

    pub(super) fn on_create(&self) {
        // A multi-line edit otherwise stops accepting text after 32 K characters.
        self.view.limit_text(None);
    }

    pub(super) fn update(&self) {
        let entries = log::since(self.tail.sequence.get());
        if let Some(latest) = entries.last() {
            self.tail.sequence.set(latest.sequence);
            self.tail.need_scroll.set(true);
            self.append(&entries);
        }
        if self.tail.need_scroll.get() && self.view.hwnd().IsWindowVisible() {
            self.scroll_to_end();
            self.tail.need_scroll.set(false);
        }
    }

    fn append(&self, entries: &[log::Entry]) {
        if entries.is_empty() {
            return;
        }
        let mut length = self.view.hwnd().GetWindowTextLength().unwrap_or(0);
        if length > VIEW_LIMIT {
            let _ = self.view.set_text("");
            length = 0;
        }
        let text: String = entries
            .iter()
            .map(|entry| entry.line().replace('\n', "\r\n") + "\r\n")
            .collect();
        self.view.set_selection(length, length);
        self.view.replace_selection(&text);
    }

    /// A read-only edit without focus does not follow its caret, and a hidden one ignores the
    /// scroll, so this runs once the tab is showing.
    #[expect(unsafe_code)]
    fn scroll_to_end(&self) {
        let length = self.view.hwnd().GetWindowTextLength().unwrap_or(0);
        self.view.set_selection(length, length);
        // SAFETY: EM_SCROLLCARET takes no parameters.
        unsafe { self.view.hwnd().SendMessage(w::msg::EmScrollCaret {}) };
    }
}

#[derive(Default)]
struct Tail {
    sequence: Cell<u64>,
    need_scroll: Cell<bool>,
}
