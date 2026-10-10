//! The modal window that asks for the NvFBC key when NvFBC is chosen as the capture backend.
//! The key NVIDIA issued is entered as hexadecimal, masked like a password, and saved to the
//! user's registry; the capture backend reads it from there each time it starts.

use super::{
    layout::{self, BLACK, LINE, RED, label},
    text,
};
use crate::{
    log,
    sys::{nvfbc_key, window},
};
use std::{cell::Cell, rc::Rc};
use winsafe::{co, gui, prelude::*};

const WIDTH: i32 = 400;
const HEIGHT: i32 = 170;
const X: i32 = 12;
const INNER_WIDTH: i32 = WIDTH - 2 * X;
const FIELD_Y: i32 = 12 + 3 * LINE + 10;
const FIELD_OFFSET: i32 = 76;
const BUTTON_WIDTH: i32 = 84;
const BUTTON_Y: i32 = HEIGHT - 12 - 24;

/// Asks for the key over `parent` until it is saved or the user cancels; reports whether a
/// key was saved.
pub(super) fn ask(parent: &impl GuiParent) -> bool {
    let saved = match nvfbc_key::load() {
        Ok(key) => key.map(|key| nvfbc_key::format(&key)).unwrap_or_default(),
        Err(error) => {
            log::warning(text::nvfbc_key_unreadable(error));
            String::new()
        }
    };

    let wnd = gui::WindowModal::new(gui::WindowModalOpts {
        title: text::NVFBC_KEY_TITLE,
        size: gui::dpi(WIDTH, HEIGHT),
        ..Default::default()
    });
    let _help = label(&wnd, text::NVFBC_KEY_HELP, X, 12, INNER_WIDTH, 3);
    let _caption = label(&wnd, text::NVFBC_KEY, X, FIELD_Y + 4, FIELD_OFFSET - 4, 1);
    let field = gui::Edit::new(
        &wnd,
        gui::EditOpts {
            text: &saved,
            position: gui::dpi(X + FIELD_OFFSET, FIELD_Y),
            width: gui::dpi_x(INNER_WIDTH - FIELD_OFFSET),
            control_style: co::ES::AUTOHSCROLL | co::ES::NOHIDESEL | co::ES::PASSWORD,
            ..Default::default()
        },
    );
    let error = label(&wnd, "", X, FIELD_Y + 30, INNER_WIDTH, 1);
    // The dialog IDs let Enter and Escape press the buttons.
    let save = gui::Button::new(
        &wnd,
        gui::ButtonOpts {
            text: text::NVFBC_KEY_SAVE,
            position: gui::dpi(WIDTH - X - 2 * BUTTON_WIDTH - 8, BUTTON_Y),
            width: gui::dpi_x(BUTTON_WIDTH),
            height: gui::dpi_y(24),
            control_style: co::BS::DEFPUSHBUTTON,
            ctrl_id: co::DLGID::OK.raw(),
            ..Default::default()
        },
    );
    let cancel = gui::Button::new(
        &wnd,
        gui::ButtonOpts {
            text: text::NVFBC_KEY_CANCEL,
            position: gui::dpi(WIDTH - X - BUTTON_WIDTH, BUTTON_Y),
            width: gui::dpi_x(BUTTON_WIDTH),
            height: gui::dpi_y(24),
            ctrl_id: co::DLGID::CANCEL.raw(),
            ..Default::default()
        },
    );

    let accepted = Rc::new(Cell::new(false));
    {
        let (wnd, field, error, accepted) =
            (wnd.clone(), field.clone(), error.clone(), accepted.clone());
        save.on().bn_clicked(move || {
            let entered = field.text().unwrap_or_default();
            match nvfbc_key::parse(&entered).map(|key| nvfbc_key::save(&key)) {
                Err(problem) => layout::set_text(&error, &problem.to_string()),
                Ok(Err(problem)) => {
                    let message = text::nvfbc_key_not_saved(problem);
                    log::error(&message);
                    layout::set_text(&error, &message);
                }
                Ok(Ok(())) => {
                    log::info(text::NVFBC_KEY_SAVED);
                    accepted.set(true);
                    wnd.close();
                }
            }
            Ok(())
        });
    }
    {
        let wnd = wnd.clone();
        cancel.on().bn_clicked(move || {
            wnd.close();
            Ok(())
        });
    }
    {
        let (created, field) = (wnd.clone(), field.clone());
        wnd.on().wm_create(move |_| {
            window::match_dialog_title_bar(created.hwnd());
            let _ = field.focus();
            Ok(0)
        });
    }
    // The error line is red; every other label is black.
    layout::draw(
        &wnd,
        wnd.on(),
        Vec::new(),
        co::COLOR::BTNFACE,
        move |hwnd| {
            if *hwnd == *error.hwnd() { RED } else { BLACK }
        },
    );

    if let Err(problem) = wnd.show_modal(parent) {
        log::error(text::nvfbc_key_window_failed(problem));
    }
    accepted.get()
}
