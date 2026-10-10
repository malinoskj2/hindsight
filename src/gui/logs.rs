//! The Logs tab: a read-only text box that receives journal entries as they are recorded.

use super::layout::{self, BLACK, GROUP_WIDTH, GROUP_X};
use crate::log;
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

/// Height of the log view, filling the page.
const VIEW_HEIGHT: i32 = 290;
/// Characters after which the view is cleared before the next batch of entries is appended;
/// older lines remain in the session log file.
const VIEW_LIMIT: i32 = 200_000;

#[derive(Clone)]
pub(super) struct LogsPage {
    page: gui::TabPage,
    view: gui::Edit,
    /// Where the view has reached in the journal.
    tail: Rc<Tail>,
}

#[derive(Default)]
struct Tail {
    /// The last journal entry copied into the view.
    sequence: Cell<u64>,
    /// Lines were appended while the tab was hidden; scroll once it shows.
    need_scroll: Cell<bool>,
}

impl LogsPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static)) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let view = gui::Edit::new(
            &page,
            gui::EditOpts {
                text: "",
                position: gui::dpi(GROUP_X, 6),
                width: gui::dpi_x(GROUP_WIDTH),
                height: gui::dpi_y(VIEW_HEIGHT),
                control_style: co::ES::MULTILINE
                    | co::ES::READONLY
                    | co::ES::AUTOVSCROLL
                    | co::ES::NOHIDESEL,
                window_style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::VSCROLL,
                ..Default::default()
            },
        );
        layout::draw(&page, page.on(), Vec::new(), co::COLOR::WINDOW, |_| BLACK);

        Self {
            page,
            view,
            tail: Rc::default(),
        }
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    /// The view fills the page and grows with it.
    pub(super) fn view(&self) -> &gui::Edit {
        &self.view
    }

    /// Finishes the parts that need the controls to exist.
    pub(super) fn on_create(&self) {
        // A multi-line edit otherwise stops accepting text after 32 K characters.
        self.view.limit_text(None);
    }

    /// Copies the journal entries recorded since the last call into the view.
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

    /// Appends entries at the end of the view, which scrolls to show them.
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

    /// Scrolls to the newest line. A read-only edit without focus does not follow its caret
    /// by itself, and a hidden one ignores the request, so this runs once the tab is showing.
    #[expect(unsafe_code)]
    fn scroll_to_end(&self) {
        let length = self.view.hwnd().GetWindowTextLength().unwrap_or(0);
        self.view.set_selection(length, length);
        // SAFETY: EM_SCROLLCARET takes no parameters and is sent to this control's own handle.
        unsafe { self.view.hwnd().SendMessage(w::msg::EmScrollCaret {}) };
    }
}
