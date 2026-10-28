//! The dialog that shows the running version's release notes, after an update and from About.

use super::{
    about,
    layout::{self, BUTTON_HEIGHT, BUTTON_WIDTH, MARGIN, button, skin_button},
    text,
    theme::{Skin, Surface},
};
use crate::{
    log,
    update::{self, Version, notes},
};
use winsafe::{co, gui, prelude::*};

const DIALOG_WIDTH: i32 = 420;
const NOTES_WIDTH: i32 = DIALOG_WIDTH - 2 * MARGIN;
const NOTES_HEIGHT: i32 = 240;
const BUTTONS_Y: i32 = MARGIN + NOTES_HEIGHT + MARGIN;
const DIALOG_HEIGHT: i32 = BUTTONS_Y + BUTTON_HEIGHT + MARGIN;

pub(super) fn show(owner: &impl GuiParent, skin: &Skin) {
    let version = Version::running();
    let dialog = gui::WindowModal::new(gui::WindowModalOpts {
        title: &text::whats_new_title(&version),
        size: gui::dpi(DIALOG_WIDTH, DIALOG_HEIGHT),
        ..Default::default()
    });
    // Created first, so it takes the focus and Enter closes the dialog.
    let close = gui::Button::new(
        &dialog,
        gui::ButtonOpts {
            text: text::WHATS_NEW_CLOSE,
            position: gui::dpi(DIALOG_WIDTH - MARGIN - BUTTON_WIDTH, BUTTONS_Y),
            width: gui::dpi_x(BUTTON_WIDTH),
            height: gui::dpi_y(BUTTON_HEIGHT),
            control_style: co::BS::DEFPUSHBUTTON,
            ctrl_id: co::DLGID::OK.raw(),
            ..Default::default()
        },
    );
    let notes =
        notes::running().map_or_else(|| text::WHATS_NEW_MISSING.to_owned(), notes::displayed);
    let _view = gui::Edit::new(
        &dialog,
        gui::EditOpts {
            text: &notes,
            position: gui::dpi(MARGIN, MARGIN),
            width: gui::dpi_x(NOTES_WIDTH),
            height: gui::dpi_y(NOTES_HEIGHT),
            control_style: co::ES::MULTILINE | co::ES::READONLY | co::ES::AUTOVSCROLL,
            window_style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::VSCROLL,
            ..Default::default()
        },
    );
    let releases = button(
        &dialog,
        text::WHATS_NEW_RELEASES,
        MARGIN,
        BUTTONS_Y,
        BUTTON_WIDTH,
    );
    skin_button(&close, skin);
    skin_button(&releases, skin);
    layout::draw(&dialog, dialog.on(), skin, Surface::Dialog, ());

    let created = dialog.clone();
    let title_skin = skin.clone();
    dialog.on().wm_create(move |_| {
        layout::match_title_bar(created.hwnd(), &title_skin);
        Ok(0)
    });
    let closing = dialog.clone();
    close.on().bn_clicked(move || {
        closing.close();
        Ok(())
    });
    // Escape arrives as the Cancel command.
    let closing = dialog.clone();
    dialog
        .on()
        .wm_command(co::DLGID::CANCEL, co::BN::CLICKED, move || {
            closing.close();
            Ok(())
        });
    releases.on().bn_clicked(|| {
        about::open_link(&update::releases_url());
        Ok(())
    });

    if let Err(error) = dialog.show_modal(owner) {
        log::warning(text::whats_new_failed(error));
    }
}
