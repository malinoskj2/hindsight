//! The modal window that asks for the NvFBC key when NvFBC is chosen as the capture backend.
//! The key NVIDIA issued is entered as hexadecimal or as four 0x-prefixed dwords, masked like
//! a password, and saved to the user's registry; the capture backend reads it from there each
//! time it starts.

use super::{
    layout::{self, BLACK, BUTTON_HEIGHT, LINE, RED, label},
    text,
    theme::{Skin, Surface},
};
use crate::{
    config::Theme,
    log,
    sys::{nvfbc_key, window},
};
use std::{cell::Cell, rc::Rc};
use winsafe::{co, gui, prelude::*};

const WIDTH: i32 = 400;
const HEIGHT: i32 = 170;
/// Gap between the window's edges and what it holds.
const PADDING: i32 = 12;
const INNER_WIDTH: i32 = WIDTH - 2 * PADDING;
/// Lines the explanation above the field takes.
const HELP_LINES: i32 = 3;
const FIELD_Y: i32 = PADDING + HELP_LINES * LINE + 10;
/// How far the field sits to the right of the start of its caption.
const FIELD_OFFSET: i32 = 76;
const BUTTON_WIDTH: i32 = 84;
const BUTTON_GAP: i32 = 8;
const BUTTON_Y: i32 = HEIGHT - PADDING - BUTTON_HEIGHT;

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
    let _help = label(
        &wnd,
        text::NVFBC_KEY_HELP,
        PADDING,
        PADDING,
        INNER_WIDTH,
        HELP_LINES,
    );
    let _caption = label(
        &wnd,
        text::NVFBC_KEY,
        PADDING,
        FIELD_Y + 4,
        FIELD_OFFSET - 4,
        1,
    );
    let field = gui::Edit::new(
        &wnd,
        gui::EditOpts {
            text: &saved,
            position: gui::dpi(PADDING + FIELD_OFFSET, FIELD_Y),
            width: gui::dpi_x(INNER_WIDTH - FIELD_OFFSET),
            control_style: co::ES::AUTOHSCROLL | co::ES::NOHIDESEL | co::ES::PASSWORD,
            ..Default::default()
        },
    );
    let error = label(&wnd, "", PADDING, FIELD_Y + 30, INNER_WIDTH, 1);
    let save = dialog_button(
        &wnd,
        text::NVFBC_KEY_SAVE,
        WIDTH - PADDING - 2 * BUTTON_WIDTH - BUTTON_GAP,
        co::DLGID::OK,
        co::BS::DEFPUSHBUTTON,
    );
    let cancel = dialog_button(
        &wnd,
        text::NVFBC_KEY_CANCEL,
        WIDTH - PADDING - BUTTON_WIDTH,
        co::DLGID::CANCEL,
        co::BS::PUSHBUTTON,
    );

    let accepted = Rc::new(Cell::new(false));
    let (save_wnd, save_field, save_error, save_accepted) =
        (wnd.clone(), field.clone(), error.clone(), accepted.clone());
    save.on().bn_clicked(move || {
        if let Err(message) = save_entered(&save_field.text().unwrap_or_default()) {
            layout::set_text(&save_error, &message);
            return Ok(());
        }
        save_accepted.set(true);
        save_wnd.close();
        Ok(())
    });
    let cancel_wnd = wnd.clone();
    cancel.on().bn_clicked(move || {
        cancel_wnd.close();
        Ok(())
    });
    let created = wnd.clone();
    wnd.on().wm_create(move |_| {
        window::match_dialog_title_bar(created.hwnd());
        let _ = field.focus();
        Ok(0)
    });
    // The error line is red; every other label is black.
    layout::draw(
        &wnd,
        wnd.on(),
        Vec::new(),
        &Skin::new(Theme::Classic),
        Surface::Dialog,
        move |hwnd| {
            if *hwnd == *error.hwnd() { RED } else { BLACK }
        },
    );

    if let Err(problem) = wnd.show_modal(parent) {
        log::error(text::nvfbc_key_window_failed(problem));
    }
    accepted.get()
}

/// A push button on the bottom row at `x`. Its dialog `id` lets Enter or Escape press it.
fn dialog_button(
    wnd: &gui::WindowModal,
    text: &str,
    x: i32,
    id: co::DLGID,
    style: co::BS,
) -> gui::Button {
    gui::Button::new(
        wnd,
        gui::ButtonOpts {
            text,
            position: gui::dpi(x, BUTTON_Y),
            width: gui::dpi_x(BUTTON_WIDTH),
            height: gui::dpi_y(BUTTON_HEIGHT),
            control_style: style,
            ctrl_id: id.raw(),
            ..Default::default()
        },
    )
}

/// Saves the key in `entered`, or gives the problem to show under the field.
fn save_entered(entered: &str) -> Result<(), String> {
    let key = nvfbc_key::parse(entered).map_err(|problem| problem.to_string())?;
    nvfbc_key::save(&key).map_err(|problem| {
        let message = text::nvfbc_key_not_saved(problem);
        log::error(&message);
        message
    })?;
    log::info(text::NVFBC_KEY_SAVED);
    Ok(())
}
