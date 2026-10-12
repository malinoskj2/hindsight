//! The window shown instead of the application when it could not start.

use super::{
    ICON_RESOURCE,
    layout::{self, BLACK, Frame, INNER_WIDTH, INNER_X, LINE, RED, WINDOW_WIDTH, label},
    text,
    theme::{Skin, Surface},
};
use crate::{APP_NAME, config::Theme, sys::window};
use anyhow::Result;
use winsafe::{gui, prelude::*};

const HEIGHT: i32 = 170;
const MESSAGE_Y: i32 = 28;
const MESSAGE_LINES: i32 = 4;

pub(super) fn show(message: &str) -> Result<()> {
    let dialog = ErrorDialog::new(message);
    dialog.events();
    dialog.draw();
    dialog
        .wnd
        .run_main(None)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("show the startup error: {error}"))
}

struct ErrorDialog {
    wnd: gui::WindowMain,
    frames: Vec<Frame>,
    message: gui::Label,
}

impl ErrorDialog {
    fn new(message: &str) -> Self {
        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: APP_NAME,
            size: gui::dpi(WINDOW_WIDTH, HEIGHT),
            class_icon: gui::Icon::Id(ICON_RESOURCE),
            ..Default::default()
        });
        let frame = Frame::new(&wnd, text::STARTUP_FAILED_TITLE, 6, 140);
        let message = label(
            &wnd,
            message,
            INNER_X,
            MESSAGE_Y,
            INNER_WIDTH,
            MESSAGE_LINES,
        );
        let _help = label(
            &wnd,
            text::STARTUP_FAILED_HELP,
            INNER_X,
            MESSAGE_Y + MESSAGE_LINES * LINE + 6,
            INNER_WIDTH,
            1,
        );
        Self {
            wnd,
            frames: vec![frame],
            message,
        }
    }

    fn events(&self) {
        let create_wnd = self.wnd.clone();
        let create_frames = self.frames.clone();
        self.wnd.on().wm_create(move |_| {
            window::match_dialog_title_bar(create_wnd.hwnd());
            layout::fit_titles(&create_frames);
            Ok(0)
        });
    }

    fn draw(&self) {
        let message = self.message.clone();
        layout::draw(
            &self.wnd,
            self.wnd.on(),
            self.frames.clone(),
            &Skin::new(Theme::Classic),
            Surface::Dialog,
            move |hwnd| {
                if *hwnd == *message.hwnd() { RED } else { BLACK }
            },
        );
    }
}
