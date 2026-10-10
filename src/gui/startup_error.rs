//! The window shown instead of the application when it could not start.

use super::{
    ICON_RESOURCE,
    layout::{self, BLACK, Frame, INNER_WIDTH, INNER_X, LINE, RED, WINDOW_WIDTH, label},
    text,
};
use crate::{APP_NAME, sys::window};
use anyhow::Result;
use winsafe::{co, gui, prelude::*};

/// Shows `message` with its explanation until the user closes the window.
pub(super) fn show(message: &str) -> Result<()> {
    let wnd = gui::WindowMain::new(gui::WindowMainOpts {
        title: APP_NAME,
        size: gui::dpi(WINDOW_WIDTH, 170),
        class_icon: gui::Icon::Id(ICON_RESOURCE),
        ..Default::default()
    });
    let group = Frame::new(&wnd, text::STARTUP_FAILED_TITLE, 6, 140);
    let message = label(&wnd, message, INNER_X, 28, INNER_WIDTH, 4);
    let _help = label(
        &wnd,
        text::STARTUP_FAILED_HELP,
        INNER_X,
        28 + 4 * LINE + 6,
        INNER_WIDTH,
        1,
    );

    let frames = vec![group];
    let (create_wnd, title_frames) = (wnd.clone(), frames.clone());
    wnd.on().wm_create(move |_| {
        window::match_dialog_title_bar(create_wnd.hwnd());
        layout::fit_titles(&title_frames);
        Ok(0)
    });
    // The error itself is red; the title and the help line are black.
    layout::draw(&wnd, wnd.on(), frames, co::COLOR::BTNFACE, move |hwnd| {
        if *hwnd == *message.hwnd() { RED } else { BLACK }
    });

    wnd.run_main(None)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("show the startup error: {error}"))
}
