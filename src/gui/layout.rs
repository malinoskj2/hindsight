//! Geometry shared by the pages, and the helpers that create, draw and move controls.

use super::theme::{Skin, Surface};
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

pub(super) const WINDOW_WIDTH: i32 = 470;
pub(super) const MARGIN: i32 = 8;
/// Both sides of the tab control's frame.
const TAB_FRAME: i32 = 8;
const PAGE_WIDTH: i32 = WINDOW_WIDTH - 2 * MARGIN - TAB_FRAME;

pub(super) const GROUP_X: i32 = 8;
pub(super) const GROUP_WIDTH: i32 = PAGE_WIDTH - 2 * GROUP_X;
const GROUP_PADDING: i32 = 12;
pub(super) const INNER_X: i32 = GROUP_X + GROUP_PADDING;
pub(super) const INNER_WIDTH: i32 = GROUP_WIDTH - 2 * GROUP_PADDING;
const CAPTION_WIDTH: i32 = 108;
pub(super) const VALUE_X: i32 = INNER_X + CAPTION_WIDTH;
pub(super) const VALUE_WIDTH: i32 = INNER_WIDTH - CAPTION_WIDTH;
const TITLE_INDENT: i32 = 8;

pub(super) const FILL_Y: i32 = 6;
/// Down to where the Settings tab's last group ends while compact; grows with the page.
pub(super) const FILL_HEIGHT: i32 = 364;

pub(super) const LINE: i32 = 18;
pub(super) const BUTTON_WIDTH: i32 = 120;
pub(super) const BUTTON_HEIGHT: i32 = 24;

pub(super) const BLACK: w::COLORREF = w::COLORREF::from_rgb(0x00, 0x00, 0x00);
pub(super) const GRAY: w::COLORREF = w::COLORREF::from_rgb(0x6D, 0x6D, 0x6D);
pub(super) const GREEN: w::COLORREF = w::COLORREF::from_rgb(0x10, 0x7C, 0x10);
pub(super) const AMBER: w::COLORREF = w::COLORREF::from_rgb(0x9D, 0x5D, 0x00);
pub(super) const RED: w::COLORREF = w::COLORREF::from_rgb(0xC4, 0x2B, 0x1C);

/// A group box the window paints itself: a child group-box control never erases its interior
/// under a clip-children parent.
#[derive(Clone)]
pub(super) struct Frame {
    title: gui::Label,
    text: String,
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
            title: label(parent, &padded, GROUP_X + TITLE_INDENT, y, 0, 1),
            text: title.to_owned(),
            rect: Rc::new(Cell::new(w::RECT {
                left: gui::dpi_x(GROUP_X),
                top: gui::dpi_y(y + LINE / 2),
                right: gui::dpi_x(GROUP_X + GROUP_WIDTH),
                bottom: gui::dpi_y(y + height),
            })),
        }
    }

    pub(super) fn rect(&self) -> w::RECT {
        self.rect.get()
    }

    pub(super) fn title(&self) -> &str {
        &self.text
    }

    pub(super) fn show_title_label(&self, shown: bool) {
        show(&self.title, shown);
    }

    fn grow(&self, delta: i32) {
        let mut rect = self.rect.get();
        rect.bottom += delta;
        self.rect.set(rect);
    }

    fn shift(&self, delta: i32) {
        let mut rect = self.rect.get();
        rect.top += delta;
        rect.bottom += delta;
        self.rect.set(rect);
        shift_by(self.title.hwnd(), delta);
    }
}

/// A label whose colour the `WM_CTLCOLORSTATIC` handler reads without borrowing anything.
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

    pub(super) fn set_colour(&self, colour: w::COLORREF) {
        if self.colour.replace(colour) != colour {
            let _ = self.label.hwnd().InvalidateRect(None, true);
        }
    }

    pub(super) fn colour_of(&self, hwnd: &w::HWND) -> Option<w::COLORREF> {
        (*hwnd == *self.label.hwnd()).then(|| self.colour.get())
    }
}

/// A group that grows while its message label shows, moving what is declared below it.
#[derive(Clone)]
pub(super) struct ExpandableGroup {
    frame: Frame,
    message: gui::Label,
    extra: i32,
    frames_below: Vec<Frame>,
    controls_below: Vec<Rc<dyn GuiWindow>>,
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

    pub(super) fn moving_frame(mut self, frame: Frame) -> Self {
        self.frames_below.push(frame);
        self
    }

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

    pub(super) fn on_create(&self) {
        show(&self.message, false);
    }

    pub(super) fn extra(&self) -> i32 {
        self.shown.get()
    }

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

pub(super) fn skin_button(button: &gui::Button, skin: &Skin) {
    let skin = skin.clone();
    let owner = button.clone();
    button.on().nm_custom_draw(move |draw| {
        let text = owner.hwnd().GetWindowText().unwrap_or_default();
        Ok(skin.paint_button(draw, &text))
    });
}

pub(super) fn fit_titles(frames: &[Frame]) {
    for frame in frames {
        if let Ok(title) = frame.title.hwnd().GetWindowText() {
            let _ = frame.title.set_text_and_resize(&title);
        }
    }
}

pub(super) fn draw(
    host: &(impl GuiParent + 'static),
    events: &impl GuiEventsParent,
    frames: Vec<Frame>,
    skin: &Skin,
    surface: Surface,
    text_colour: impl Fn(&w::HWND) -> w::COLORREF + 'static,
) {
    let erase_host = host.clone();
    let erase_skin = skin.clone();
    let erase_frames = frames.clone();
    events.wm_erase_bkgnd(move |p| {
        erase(
            &p.hdc,
            erase_host.hwnd(),
            &erase_skin,
            surface,
            &erase_frames,
        );
        Ok(1)
    });
    if surface == Surface::Page || !frames.is_empty() {
        let paint_host = host.clone();
        let paint_skin = skin.clone();
        events.wm_paint(move || {
            paint_page(paint_host.hwnd(), &paint_skin, &frames);
            Ok(())
        });
    }
    let label_skin = skin.clone();
    events.wm_ctl_color_static(move |p| {
        let _ = p
            .hdc
            .SetTextColor(label_skin.text_colour(text_colour(&p.hwnd)));
        let _ = p.hdc.SetBkMode(co::BKMODE::TRANSPARENT);
        Ok(label_skin.label_brush(surface))
    });
}

fn erase(hdc: &w::HDC, host: &w::HWND, skin: &Skin, surface: Surface, frames: &[Frame]) {
    if !skin.is_themed() {
        if let Ok(client) = host.GetClientRect()
            && let Ok(brush) = w::HBRUSH::GetSysColorBrush(surface.classic())
        {
            let _ = hdc.FillRect(client, &brush);
        }
        return;
    }
    match surface {
        Surface::Dialog => skin.fill_dialog(hdc, host),
        // The page paints itself in one go; only a child asking for its backdrop is erased here.
        Surface::Page if hdc.WindowFromDC().as_ref() == Some(host) => {}
        Surface::Page => skin.paint_backdrop(hdc, host, frames),
    }
}

fn paint_page(hwnd: &w::HWND, skin: &Skin, frames: &[Frame]) {
    let Ok(paint) = hwnd.BeginPaint() else {
        return;
    };
    if skin.is_themed() {
        skin.paint_page(&paint, hwnd, frames);
        return;
    }
    let Some(theme) = hwnd.OpenThemeData("BUTTON") else {
        return;
    };
    for frame in frames {
        let _ =
            theme.DrawThemeBackground(&paint, co::VS::BUTTON_GROUPBOX_NORMAL, frame.rect(), None);
    }
}

pub(super) fn show(control: &impl GuiWindow, visible: bool) {
    control
        .hwnd()
        .ShowWindow(if visible { co::SW::SHOW } else { co::SW::HIDE });
}

pub(super) fn set_text(control: &impl GuiWindow, value: &str) {
    if control.hwnd().GetWindowText().as_deref() != Ok(value) {
        let _ = control.hwnd().SetWindowText(value);
    }
}

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

pub(super) fn resize_width_by(window: &w::HWND, delta: i32) {
    add_to_size(window, delta, 0);
}

pub(super) fn resize_height_by(window: &w::HWND, delta: i32) {
    add_to_size(window, 0, delta);
}

fn add_to_size(window: &w::HWND, width: i32, height: i32) {
    let Ok(rect) = window.GetWindowRect() else {
        return;
    };
    let _ = window.SetWindowPos(
        w::HwndPlace::None,
        w::POINT::default(),
        w::SIZE::with(
            rect.right - rect.left + width,
            rect.bottom - rect.top + height,
        ),
        co::SWP::NOMOVE | co::SWP::NOZORDER | co::SWP::NOACTIVATE | co::SWP::NOCOPYBITS,
    );
}
