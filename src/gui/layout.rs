//! Geometry shared by the pages, and the helpers that create, draw and move controls.

use super::theme::{Skin, Surface};
use crate::APP_NAME;
use anyhow::Result;
use std::{cell::Cell, iter, rc::Rc};
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
const TITLE_LABEL_INDENT: i32 = 8;

pub(super) const FILL_Y: i32 = 6;
/// Down to where the Settings tab's last group ends while compact; grows with the page.
pub(super) const FILL_HEIGHT: i32 = 364;
/// The tab strip above the pages and the slack under their content.
const TAB_CHROME: i32 = 36;
pub(super) const TAB_HEIGHT: i32 = FILL_Y + FILL_HEIGHT + TAB_CHROME;
pub(super) const WINDOW_HEIGHT: i32 = MARGIN + TAB_HEIGHT + MARGIN;

/// The DPI the layout is laid out at before `gui::dpi` scales it.
const BASE_DPI: u32 = 96;

pub(super) const LINE: i32 = 18;
pub(super) const BUTTON_WIDTH: i32 = 120;
pub(super) const BUTTON_HEIGHT: i32 = 24;

pub(super) const BLACK: w::COLORREF = w::COLORREF::from_rgb(0x00, 0x00, 0x00);
pub(super) const GRAY: w::COLORREF = w::COLORREF::from_rgb(0x6D, 0x6D, 0x6D);
pub(super) const GREEN: w::COLORREF = w::COLORREF::from_rgb(0x10, 0x7C, 0x10);
pub(super) const AMBER: w::COLORREF = w::COLORREF::from_rgb(0x9D, 0x5D, 0x00);
pub(super) const RED: w::COLORREF = w::COLORREF::from_rgb(0xC4, 0x2B, 0x1C);

/// What a window's paint handlers read back from it: the frames it paints itself and the
/// labels whose text has a colour of its own.
pub(super) trait Painted {
    fn frames(&self) -> &[Frame];
    fn tinted(&self) -> impl Iterator<Item = &TintedLabel>;
}

impl Painted for () {
    fn frames(&self) -> &[Frame] {
        &[]
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        iter::empty()
    }
}

impl<P: Painted> Painted for Rc<P> {
    fn frames(&self) -> &[Frame] {
        (**self).frames()
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        (**self).tinted()
    }
}

/// A group box the window paints itself: a child group-box control never erases its interior
/// under a clip-children parent.
pub(super) struct Frame {
    label: gui::Label,
    title: String,
    rect: Cell<w::RECT>,
}

impl Frame {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        title: &str,
        y: i32,
        height: i32,
    ) -> Self {
        Self {
            label: label(
                parent,
                &padded(title),
                GROUP_X + TITLE_LABEL_INDENT,
                y,
                0,
                1,
            ),
            title: title.to_owned(),
            rect: Cell::new(w::RECT {
                left: gui::dpi_x(GROUP_X),
                top: gui::dpi_y(y + LINE / 2),
                right: gui::dpi_x(GROUP_X + GROUP_WIDTH),
                bottom: gui::dpi_y(y + height),
            }),
        }
    }

    pub(super) fn rect(&self) -> w::RECT {
        self.rect.get()
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    pub(super) fn show_title_label(&self, shown: bool) {
        show(&self.label, shown);
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
        shift_by(self.label.hwnd(), delta);
    }
}

/// A label whose colour the `WM_CTLCOLORSTATIC` handler reads without borrowing anything.
pub(super) struct TintedLabel {
    label: gui::Label,
    colour: Cell<w::COLORREF>,
}

impl TintedLabel {
    pub(super) fn new(label: gui::Label, colour: w::COLORREF) -> Self {
        Self {
            label,
            colour: Cell::new(colour),
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

    fn colour_of(&self, hwnd: &w::HWND) -> Option<w::COLORREF> {
        (*hwnd == *self.label.hwnd()).then(|| self.colour.get())
    }
}

/// A group that grows while its message label shows, moving what is declared below it.
pub(super) struct ExpandableGroup {
    /// The growing frame's place among the page's frames; those after it move down.
    frame: usize,
    message: gui::Label,
    extra: i32,
    controls_below: Vec<Movable>,
    shown: Cell<i32>,
}

/// A control under an expandable group, moved down when the group grows.
pub(super) enum Movable {
    Label(gui::Label),
    Button(gui::Button),
}

impl Movable {
    fn hwnd(&self) -> &w::HWND {
        match self {
            Self::Label(label) => label.hwnd(),
            Self::Button(button) => button.hwnd(),
        }
    }
}

impl From<gui::Label> for Movable {
    fn from(label: gui::Label) -> Self {
        Self::Label(label)
    }
}

impl From<gui::Button> for Movable {
    fn from(button: gui::Button) -> Self {
        Self::Button(button)
    }
}

impl ExpandableGroup {
    pub(super) fn new(frame: usize, message: gui::Label, extra: i32) -> Self {
        Self {
            frame,
            message,
            extra,
            controls_below: Vec::new(),
            shown: Cell::new(0),
        }
    }

    pub(super) fn moving(mut self, controls: impl IntoIterator<Item = impl Into<Movable>>) -> Self {
        self.controls_below
            .extend(controls.into_iter().map(Into::into));
        self
    }

    pub(super) fn extra(&self) -> i32 {
        self.shown.get()
    }

    pub(super) fn set_message(&self, frames: &[Frame], message: Option<&str>) -> bool {
        set_text(&self.message, message.unwrap_or(""));
        let extra = if message.is_some() { self.extra } else { 0 };
        let previous = self.shown.replace(extra);
        if previous == extra {
            return false;
        }
        show(&self.message, extra > 0);
        let delta = gui::dpi_y(extra - previous);
        let mut from_grown = frames.iter().skip(self.frame);
        if let Some(frame) = from_grown.next() {
            frame.grow(delta);
        }
        for frame in from_grown {
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

pub(super) fn window_opts(width: i32, height: i32, icon: u16) -> gui::WindowMainOpts<'static> {
    gui::WindowMainOpts {
        title: APP_NAME,
        size: gui::dpi(width, height),
        class_icon: gui::Icon::Id(icon),
        ..Default::default()
    }
}

pub(super) fn run(window: &gui::WindowMain, show: Option<co::SW>, context: &str) -> Result<()> {
    window
        .run_main(show)
        .map(|_| ())
        .map_err(|error| anyhow::Error::from_boxed(error).context(context.to_owned()))
}

pub(super) fn page(parent: &(impl GuiParent + 'static)) -> gui::TabPage {
    gui::TabPage::new(parent, gui::TabPageOpts::default())
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

/// Shown once it has something to say.
pub(super) fn hidden_label(
    parent: &(impl GuiParent + 'static),
    x: i32,
    y: i32,
    width: i32,
    lines: i32,
) -> gui::Label {
    gui::Label::new(
        parent,
        gui::LabelOpts {
            text: "",
            position: gui::dpi(x, y),
            size: gui::dpi(width, LINE * lines),
            window_style: co::WS::CHILD,
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

/// Sizes each title label to its text, which takes a created window.
pub(super) fn fit_titles(frames: &[Frame]) {
    for frame in frames {
        let _ = frame.label.set_text_and_resize(&padded(&frame.title));
    }
}

/// Spaces on either side keep the group's line off the text.
fn padded(title: &str) -> String {
    format!(" {title} ")
}

pub(super) fn draw(
    host: &(impl GuiParent + 'static),
    events: &impl GuiEventsParent,
    skin: &Skin,
    surface: Surface,
    painted: impl Painted + Clone + 'static,
) {
    let erase_host = host.clone();
    let erase_skin = skin.clone();
    let erase_painted = painted.clone();
    events.wm_erase_bkgnd(move |params| {
        erase(
            &params.hdc,
            erase_host.hwnd(),
            &erase_skin,
            surface,
            erase_painted.frames(),
        );
        Ok(1)
    });
    if surface == Surface::Page || !painted.frames().is_empty() {
        let paint_host = host.clone();
        let paint_skin = skin.clone();
        let paint_painted = painted.clone();
        events.wm_paint(move || {
            paint_page(paint_host.hwnd(), &paint_skin, paint_painted.frames());
            Ok(())
        });
    }
    let label_skin = skin.clone();
    events.wm_ctl_color_static(move |params| {
        let classic = painted
            .tinted()
            .find_map(|label| label.colour_of(&params.hwnd))
            .unwrap_or(BLACK);
        let _ = params.hdc.SetTextColor(label_skin.text_colour(classic));
        let _ = params.hdc.SetBkMode(co::BKMODE::TRANSPARENT);
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

/// Lengths and counts past `i32::MAX` are taken as that.
pub(super) fn saturating_i32(value: impl TryInto<i32>) -> i32 {
    value.try_into().unwrap_or(i32::MAX)
}

/// On the scale of `gui::dpi_x`, like every other width here.
pub(super) fn scroll_bar_width() -> i32 {
    w::GetSystemMetricsForDpi(co::SM::CXVSCROLL, BASE_DPI)
        .map_or_else(|_| w::GetSystemMetrics(co::SM::CXVSCROLL), gui::dpi_x)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_and_window_are_as_tall_as_they_were_laid_by_hand() {
        assert_eq!((TAB_HEIGHT, WINDOW_HEIGHT), (406, 422));
    }
}
