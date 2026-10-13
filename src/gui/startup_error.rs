//! The window shown instead of the application when it could not start.

use super::{
    layout::{self, Frame, INNER_WIDTH, INNER_X, LINE, Painted, TintedLabel, WINDOW_WIDTH, label},
    text,
    theme::{Skin, Surface, Tone},
};
use crate::{config::Theme, sys::window};
use anyhow::Result;
use std::{iter, rc::Rc};
use winsafe::{gui, prelude::*};

const DIALOG_HEIGHT: i32 = 170;
const MESSAGE_Y: i32 = 28;
const MESSAGE_LINES: i32 = 4;

pub(super) fn show(message: &str, icon: u16) -> Result<()> {
    let dialog = ErrorDialog::new(message, icon);
    dialog.events();
    dialog.draw();
    layout::run(&dialog.0.main_window, None, "show the startup error")
}

#[derive(Clone)]
struct ErrorDialog(Rc<Inner>);

struct Inner {
    main_window: gui::WindowMain,
    frames: Vec<Frame>,
    message: TintedLabel,
}

impl Painted for Inner {
    fn frames(&self) -> &[Frame] {
        &self.frames
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        iter::once(&self.message)
    }
}

impl ErrorDialog {
    fn new(message: &str, icon: u16) -> Self {
        let main_window =
            gui::WindowMain::new(layout::window_opts(WINDOW_WIDTH, DIALOG_HEIGHT, icon));
        let frame = Frame::new(&main_window, text::STARTUP_FAILED_TITLE, 6, 140);
        let message = label(
            &main_window,
            message,
            INNER_X,
            MESSAGE_Y,
            INNER_WIDTH,
            MESSAGE_LINES,
        );
        let _help = label(
            &main_window,
            text::STARTUP_FAILED_HELP,
            INNER_X,
            MESSAGE_Y + MESSAGE_LINES * LINE + 6,
            INNER_WIDTH,
            1,
        );
        Self(Rc::new(Inner {
            main_window,
            frames: vec![frame],
            message: TintedLabel::new(message, Tone::Error),
        }))
    }

    fn events(&self) {
        let dialog = self.clone();
        self.0.main_window.on().wm_create(move |_| {
            window::match_dialog_title_bar(dialog.0.main_window.hwnd());
            layout::fit_titles(&dialog.0.frames);
            Ok(0)
        });
    }

    fn draw(&self) {
        layout::draw(
            &self.0.main_window,
            self.0.main_window.on(),
            &Skin::new(Theme::Classic),
            Surface::Dialog,
            Rc::clone(&self.0),
        );
    }
}
