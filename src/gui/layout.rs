//! Geometry shared by the pages and the helpers that create, draw and move controls. Positions
//! are hand-laid in logical pixels; `winsafe::gui::dpi` scales them when a control is created.

use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

pub(super) const WINDOW_WIDTH: i32 = 470;
pub(super) const MARGIN: i32 = 8;
/// Pitch of a row of text.
pub(super) const LINE: i32 = 18;
const BUTTON_HEIGHT: i32 = 24;
const PAGE_WIDTH: i32 = WINDOW_WIDTH - 2 * MARGIN - 8;
pub(super) const GROUP_X: i32 = 8;
pub(super) const GROUP_WIDTH: i32 = PAGE_WIDTH - 2 * GROUP_X;
pub(super) const INNER_X: i32 = GROUP_X + 12;
pub(super) const INNER_WIDTH: i32 = GROUP_WIDTH - 24;
pub(super) const VALUE_X: i32 = INNER_X + 108;
pub(super) const VALUE_WIDTH: i32 = INNER_WIDTH - 108;

pub(super) const BLACK: w::COLORREF = w::COLORREF::from_rgb(0x00, 0x00, 0x00);
pub(super) const GRAY: w::COLORREF = w::COLORREF::from_rgb(0x6D, 0x6D, 0x6D);
pub(super) const GREEN: w::COLORREF = w::COLORREF::from_rgb(0x10, 0x7C, 0x10);
pub(super) const AMBER: w::COLORREF = w::COLORREF::from_rgb(0x9D, 0x5D, 0x00);
pub(super) const RED: w::COLORREF = w::COLORREF::from_rgb(0xC4, 0x2B, 0x1C);

/// A group box: the themed frame is painted by the window itself (a child group-box control
/// never erases its interior, which leaves stale pixels under a clip-children parent) and the
/// title is an ordinary label sitting on the frame line.
#[derive(Clone)]
pub(super) struct Frame {
    title: gui::Label,
    /// In physical pixels; shared between the paint handler and the group that moves it.
    rect: Rc<Cell<w::RECT>>,
}

impl Frame {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        title: &str,
        y: i32,
        height: i32,
    ) -> Self {
        let padded = format!(" {title} ");
        Self {
            title: label(parent, &padded, GROUP_X + 8, y, 0, 1),
            rect: Rc::new(Cell::new(w::RECT {
                left: gui::dpi_x(GROUP_X),
                top: gui::dpi_y(y + LINE / 2),
                right: gui::dpi_x(GROUP_X + GROUP_WIDTH),
                bottom: gui::dpi_y(y + height),
            })),
        }
    }

    /// Makes the frame `delta` physical pixels taller (shorter when negative).
    fn grow(&self, delta: i32) {
        let mut rect = self.rect.get();
        rect.bottom += delta;
        self.rect.set(rect);
    }

    /// Moves the frame and its title down by `delta` physical pixels (up when negative).
    fn shift(&self, delta: i32) {
        let mut rect = self.rect.get();
        rect.top += delta;
        rect.bottom += delta;
        self.rect.set(rect);
        shift_by(self.title.hwnd(), delta);
    }
}

/// Sizes the title labels to their text; only possible once the controls exist.
pub(super) fn fit_titles(frames: &[Frame]) {
    for frame in frames {
        if let Ok(title) = frame.title.hwnd().GetWindowText() {
            let _ = frame.title.set_text_and_resize(&title);
        }
    }
}

/// Installs the handlers that draw a window's own parts: `frames`, if any, in `WM_PAINT`, and
/// in `WM_CTLCOLORSTATIC` the colour `text_colour` chooses for each label over `background`.
pub(super) fn draw(
    host: &(impl GuiParent + 'static),
    events: &impl GuiEventsParent,
    frames: Vec<Frame>,
    background: co::COLOR,
    text_colour: impl Fn(&w::HWND) -> w::COLORREF + 'static,
) {
    if !frames.is_empty() {
        let paint_host = host.clone();
        events.wm_paint(move || {
            paint_frames(paint_host.hwnd(), &frames);
            Ok(())
        });
    }
    events.wm_ctl_color_static(move |p| {
        let _ = p.hdc.SetTextColor(text_colour(&p.hwnd));
        let _ = p.hdc.SetBkMode(co::BKMODE::TRANSPARENT);
        Ok(w::HBRUSH::GetSysColorBrush(background).unwrap_or(w::HBRUSH::NULL))
    });
}

fn paint_frames(hwnd: &w::HWND, frames: &[Frame]) {
    let Ok(paint) = hwnd.BeginPaint() else {
        return;
    };
    if let Some(theme) = hwnd.OpenThemeData("BUTTON") {
        for frame in frames {
            let _ = theme.DrawThemeBackground(
                &paint,
                co::VS::BUTTON_GROUPBOX_NORMAL,
                frame.rect.get(),
                None,
            );
        }
    }
}

/// A label whose text colour carries meaning. The colour is kept beside the control so the
/// window's colour handler, which runs during painting, reads it without borrowing anything.
#[derive(Clone)]
pub(super) struct TintedLabel {
    label: gui::Label,
    colour: Rc<Cell<w::COLORREF>>,
}

impl TintedLabel {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        x: i32,
        y: i32,
        width: i32,
        lines: i32,
        colour: w::COLORREF,
    ) -> Self {
        Self {
            label: label(parent, "", x, y, width, lines),
            colour: Rc::new(Cell::new(colour)),
        }
    }

    pub(super) fn label(&self) -> &gui::Label {
        &self.label
    }

    pub(super) fn set_text(&self, value: &str) {
        set_text(&self.label, value);
    }

    /// Repaints the label when its colour changes.
    pub(super) fn set_colour(&self, colour: w::COLORREF) {
        if self.colour.replace(colour) != colour {
            let _ = self.label.hwnd().InvalidateRect(None, true);
        }
    }

    /// The label's current colour when `hwnd` is this label.
    pub(super) fn colour_of(&self, hwnd: &w::HWND) -> Option<w::COLORREF> {
        (*hwnd == *self.label.hwnd()).then(|| self.colour.get())
    }
}

/// A group that stays compact until its message label has something to say: the frame then
/// grows by a fixed number of lines to make room, everything declared below it moves down,
/// and both move back once the message clears. Fitting the window around the taller page is
/// left to the caller, which `set_message` tells when the height changed.
#[derive(Clone)]
pub(super) struct ExpandableGroup {
    frame: Frame,
    message: gui::Label,
    /// Height the group gains while the message is shown, in logical pixels.
    extra: i32,
    frames_below: Vec<Frame>,
    controls_below: Vec<Rc<dyn GuiWindow>>,
    /// The extra height currently shown, in logical pixels.
    shown: Rc<Cell<i32>>,
}

impl ExpandableGroup {
    pub(super) fn new(frame: Frame, message: gui::Label, extra: i32) -> Self {
        Self {
            frame,
            message,
            extra,
            frames_below: Vec::new(),
            controls_below: Vec::new(),
            shown: Rc::new(Cell::new(0)),
        }
    }

    /// Declares a frame below the group that moves when it grows.
    pub(super) fn moving_frame(mut self, frame: Frame) -> Self {
        self.frames_below.push(frame);
        self
    }

    /// Declares controls below the group that move when it grows.
    pub(super) fn moving<C: GuiWindow + 'static>(
        mut self,
        controls: impl IntoIterator<Item = C>,
    ) -> Self {
        self.controls_below.extend(
            controls
                .into_iter()
                .map(|control| Rc::new(control) as Rc<dyn GuiWindow>),
        );
        self
    }

    /// Hides the message label; only possible once the controls exist.
    pub(super) fn on_create(&self) {
        show(&self.message, false);
    }

    /// The extra height currently shown, in logical pixels.
    pub(super) fn extra(&self) -> i32 {
        self.shown.get()
    }

    /// Shows `message`, or nothing, and reports whether the group's height changed.
    pub(super) fn set_message(&self, message: Option<&str>) -> bool {
        set_text(&self.message, message.unwrap_or(""));
        let extra = if message.is_some() { self.extra } else { 0 };
        let previous = self.shown.replace(extra);
        if previous == extra {
            return false;
        }
        show(&self.message, extra > 0);
        let delta = gui::dpi_y(extra - previous);
        self.frame.grow(delta);
        for frame in &self.frames_below {
            frame.shift(delta);
        }
        for control in &self.controls_below {
            shift_by(control.hwnd(), delta);
        }
        if let Ok(page) = self.message.hwnd().GetParent() {
            let _ = page.InvalidateRect(None, true);
        }
        true
    }
}

pub(super) fn label(
    parent: &(impl GuiParent + 'static),
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    lines: i32,
) -> gui::Label {
    gui::Label::new(
        parent,
        gui::LabelOpts {
            text,
            position: gui::dpi(x, y),
            size: gui::dpi(width, LINE * lines),
            ..Default::default()
        },
    )
}

/// A single-line label that shortens a long path in the middle instead of clipping it.
pub(super) fn path_label(
    parent: &(impl GuiParent + 'static),
    x: i32,
    y: i32,
    width: i32,
) -> gui::Label {
    gui::Label::new(
        parent,
        gui::LabelOpts {
            text: "",
            position: gui::dpi(x, y),
            size: gui::dpi(width, LINE),
            control_style: co::SS::LEFT | co::SS::PATHELLIPSIS,
            ..Default::default()
        },
    )
}

pub(super) fn button(
    parent: &(impl GuiParent + 'static),
    text: &str,
    x: i32,
    y: i32,
    width: i32,
) -> gui::Button {
    gui::Button::new(
        parent,
        gui::ButtonOpts {
            text,
            position: gui::dpi(x, y),
            width: gui::dpi_x(width),
            height: gui::dpi_y(BUTTON_HEIGHT),
            ..Default::default()
        },
    )
}

pub(super) fn show(control: &impl GuiWindow, visible: bool) {
    control
        .hwnd()
        .ShowWindow(if visible { co::SW::SHOW } else { co::SW::HIDE });
}

/// Sets a control's text when it differs, which spares the repaint otherwise.
pub(super) fn set_text(control: &impl GuiWindow, value: &str) {
    if control.hwnd().GetWindowText().as_deref() != Ok(value) {
        let _ = control.hwnd().SetWindowText(value);
    }
}

/// Moves a child window down by `delta` physical pixels (up when negative).
pub(super) fn shift_by(window: &w::HWND, delta: i32) {
    let Ok(parent) = window.GetParent() else {
        return;
    };
    let Ok(rect) = window
        .GetWindowRect()
        .and_then(|rect| parent.ScreenToClientRc(rect))
    else {
        return;
    };
    let _ = window.SetWindowPos(
        w::HwndPlace::None,
        w::POINT::with(rect.left, rect.top + delta),
        w::SIZE::default(),
        co::SWP::NOSIZE | co::SWP::NOZORDER | co::SWP::NOACTIVATE | co::SWP::NOCOPYBITS,
    );
}

/// Makes a window `delta` physical pixels taller (shorter when negative), keeping its position.
pub(super) fn resize_by(window: &w::HWND, delta: i32) {
    let Ok(rect) = window.GetWindowRect() else {
        return;
    };
    let _ = window.SetWindowPos(
        w::HwndPlace::None,
        w::POINT::default(),
        w::SIZE::with(rect.right - rect.left, rect.bottom - rect.top + delta),
        co::SWP::NOMOVE | co::SWP::NOZORDER | co::SWP::NOACTIVATE | co::SWP::NOCOPYBITS,
    );
}
