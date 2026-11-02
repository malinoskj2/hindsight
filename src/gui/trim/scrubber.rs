//! The trim view's timeline: the clip as a groove, the cut on it between two markers that drag,
//! and the playhead, which a click or a drag moves. The view decides where each lands.

use crate::{
    gui::theme::{Skin, TimelineColours, buffered},
    sys::callback::Slot,
    trim::Cut,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use winsafe::{self as w, co, guard::ReleaseCaptureGuard, gui, prelude::*};

/// Room at either end, so the markers there can be grabbed.
const INSET: i32 = 8;
const GROOVE: i32 = 4;
const CUT: i32 = 10;
const MARKER: i32 = 3;
const MARKER_TAB: i32 = 6;
const PLAYHEAD: i32 = 2;
const PLAYHEAD_ARROW: i32 = 5;
/// How near a marker a press grabs it rather than moving the playhead.
const GRAB: i32 = 5;

/// What the pointer asks for, as a time in clock ticks from the first picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gesture {
    Scrub(i64),
    Start(i64),
    End(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key {
    PlayOrPause,
    /// A picture back or forward.
    Step(isize),
    /// A second back or forward.
    Jump(isize),
    First,
    Last,
    MarkStart,
    MarkEnd,
    Leave,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Playhead,
    Start,
    End,
}

#[derive(Clone, Copy, Default)]
struct Shown {
    duration: i64,
    position: i64,
    cut: Option<Cut>,
}

#[derive(Clone)]
pub(super) struct Scrubber(Rc<Inner>);

struct Inner {
    control: gui::WindowControl,
    skin: Skin,
    shown: Cell<Shown>,
    drag: Cell<Option<Drag>>,
    capture: RefCell<Option<ReleaseCaptureGuard>>,
    gesture: Slot<dyn Fn(Gesture)>,
    key: Slot<dyn Fn(Key)>,
}

impl Scrubber {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        position: (i32, i32),
        size: (i32, i32),
        skin: &Skin,
    ) -> Self {
        let control = gui::WindowControl::new(
            parent,
            gui::WindowControlOpts {
                position: gui::dpi(position.0, position.1),
                size: gui::dpi(size.0, size.1),
                style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::CLIPSIBLINGS,
                ..Default::default()
            },
        );
        let scrubber = Self(Rc::new(Inner {
            control,
            skin: skin.clone(),
            shown: Cell::default(),
            drag: Cell::new(None),
            capture: RefCell::new(None),
            gesture: Slot::new(),
            key: Slot::new(),
        }));
        scrubber.events();
        scrubber
    }

    pub(super) fn hwnd(&self) -> &w::HWND {
        self.0.control.hwnd()
    }

    pub(super) fn on_gesture(&self, handler: impl Fn(Gesture) + 'static) {
        self.0.gesture.set(Rc::new(handler));
    }

    pub(super) fn on_key(&self, handler: impl Fn(Key) + 'static) {
        self.0.key.set(Rc::new(handler));
    }

    /// Shows the playhead at `position` and the cut, if there is one, on a clip of `duration`.
    pub(super) fn show(&self, duration: i64, position: i64, cut: Option<Cut>) {
        self.0.shown.set(Shown {
            duration,
            position,
            cut,
        });
        let _ = self.hwnd().InvalidateRect(None, false);
    }

    fn events(&self) {
        let events = self.0.control.on();
        events.wm_erase_bkgnd(|_| Ok(1));
        let scrubber = self.clone();
        events.wm_paint(move || {
            scrubber.paint();
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_l_button_down(move |press| {
            scrubber.press(press.coords.x);
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_mouse_move(move |moved| {
            if let Some(drag) = scrubber.0.drag.get() {
                scrubber.drag_to(drag, moved.coords.x);
            }
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_l_button_up(move |_| {
            scrubber.release();
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_capture_changed(move |_| {
            scrubber.0.drag.set(None);
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_set_cursor(move |cursor| {
            Ok(cursor.hit_test == co::HT::CLIENT && scrubber.show_resize_cursor())
        });
        let scrubber = self.clone();
        events.wm_set_focus(move |_| {
            let _ = scrubber.hwnd().InvalidateRect(None, false);
            Ok(())
        });
        let scrubber = self.clone();
        events.wm_kill_focus(move |_| {
            let _ = scrubber.hwnd().InvalidateRect(None, false);
            Ok(())
        });
        events.wm_get_dlg_code(|code| {
            let escape = code.msg.as_ref().is_some_and(|message| {
                message.message == co::WM::KEYDOWN
                    && message.wParam == usize::from(co::VK::ESCAPE.raw())
            });
            Ok(dialog_code(escape))
        });
        let scrubber = self.clone();
        events.wm_key_down(move |key| {
            if let Some(key) = key_of(key.vkey_code)
                && let Some(handler) = scrubber.0.key.get()
            {
                handler(key);
            }
            Ok(())
        });
    }

    fn press(&self, x: i32) {
        let _ = self.hwnd().SetFocus();
        let drag = self.marker_at(x).unwrap_or(Drag::Playhead);
        self.0.drag.set(Some(drag));
        let guard = self.hwnd().SetCapture();
        *self.0.capture.borrow_mut() = Some(guard);
        self.drag_to(drag, x);
    }

    fn release(&self) {
        self.0.drag.set(None);
        // Taken out first: releasing the capture sends WM_CAPTURECHANGED at once.
        let guard = self.0.capture.borrow_mut().take();
        drop(guard);
    }

    fn drag_to(&self, drag: Drag, x: i32) {
        let time = self.time_at(x);
        let gesture = match drag {
            Drag::Playhead => Gesture::Scrub(time),
            Drag::Start => Gesture::Start(time),
            Drag::End => Gesture::End(time),
        };
        if let Some(handler) = self.0.gesture.get() {
            handler(gesture);
        }
    }

    /// The marker under `x`, the nearer when both are.
    fn marker_at(&self, x: i32) -> Option<Drag> {
        let cut = self.0.shown.get().cut?;
        let grab = gui::dpi_x(GRAB);
        let (start, end) = (self.x_of(cut.start), self.x_of(cut.end));
        let (to_start, to_end) = ((x - start).abs(), (x - end).abs());
        if to_start > grab && to_end > grab {
            None
        } else if to_end < to_start || (to_end == to_start && x > end) {
            Some(Drag::End)
        } else {
            Some(Drag::Start)
        }
    }

    #[expect(unsafe_code, reason = "calls LoadCursorW and SetCursor")]
    fn show_resize_cursor(&self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{IDC_SIZEWE, LoadCursorW, SetCursor};

        let over = match self.0.drag.get() {
            Some(drag) => drag != Drag::Playhead,
            None => w::GetCursorPos()
                .and_then(|point| self.hwnd().ScreenToClient(point))
                .is_ok_and(|point| self.marker_at(point.x).is_some()),
        };
        if !over {
            return false;
        }
        // SAFETY: a system cursor, which is shared and never freed.
        if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_SIZEWE) } {
            // SAFETY: the cursor was just loaded and stays valid.
            unsafe { SetCursor(Some(cursor)) };
        }
        true
    }

    fn track(&self) -> (i32, i32) {
        let width = self.hwnd().GetClientRect().map_or(0, |client| client.right);
        let inset = gui::dpi_x(INSET);
        (inset, (width - inset).max(inset + 1))
    }

    fn x_of(&self, time: i64) -> i32 {
        let (left, right) = self.track();
        let duration = self.0.shown.get().duration.max(1);
        let span = i64::from(right - left);
        let offset = time.clamp(0, duration) * span / duration;
        left + i32::try_from(offset).unwrap_or(0)
    }

    fn time_at(&self, x: i32) -> i64 {
        let (left, right) = self.track();
        let duration = self.0.shown.get().duration;
        let offset = i64::from((x - left).clamp(0, right - left));
        offset * duration / i64::from(right - left)
    }

    fn paint(&self) {
        let hwnd = self.hwnd();
        let Ok(paint) = hwnd.BeginPaint() else {
            return;
        };
        let Ok(client) = hwnd.GetClientRect() else {
            return;
        };
        let colours = self.0.skin.timeline_colours();
        let focused = w::HWND::GetFocus().as_ref() == Some(hwnd);
        buffered(&paint, (client.right, client.bottom), None, |hdc| {
            self.draw(hdc, client, colours, focused);
        });
    }

    fn draw(&self, hdc: &w::HDC, client: w::RECT, colours: TimelineColours, focused: bool) {
        let shown = self.0.shown.get();
        fill(hdc, client, colours.background);
        let middle = client.bottom / 2;
        let (left, right) = self.track();
        let band = |height: i32| {
            let half = gui::dpi_y(height) / 2;
            (middle - half, middle + half.max(1))
        };
        let (top, bottom) = band(GROOVE);
        fill(hdc, rect(left, top, right, bottom), colours.groove);
        if let Some(cut) = shown.cut {
            let (start, end) = (self.x_of(cut.start), self.x_of(cut.end));
            let (top, bottom) = band(CUT);
            fill(
                hdc,
                rect(start, top, end.max(start + 1), bottom),
                colours.cut,
            );
            let marker = gui::dpi_x(MARKER);
            let tab = gui::dpi_x(MARKER_TAB);
            let (high, low) = (gui::dpi_y(4), client.bottom - gui::dpi_y(4));
            fill(
                hdc,
                rect(start - marker / 2, high, start - marker / 2 + marker, low),
                colours.cut,
            );
            fill(
                hdc,
                rect(start, high, start + tab, high + marker),
                colours.cut,
            );
            fill(
                hdc,
                rect(start, low - marker, start + tab, low),
                colours.cut,
            );
            fill(
                hdc,
                rect(end - marker / 2, high, end - marker / 2 + marker, low),
                colours.cut,
            );
            fill(hdc, rect(end - tab, high, end, high + marker), colours.cut);
            fill(hdc, rect(end - tab, low - marker, end, low), colours.cut);
        }
        let at = self.x_of(shown.position);
        let width = gui::dpi_x(PLAYHEAD);
        fill(
            hdc,
            rect(
                at - width / 2,
                gui::dpi_y(2),
                at - width / 2 + width,
                client.bottom - gui::dpi_y(2),
            ),
            colours.playhead,
        );
        let arrow = gui::dpi_x(PLAYHEAD_ARROW);
        triangle(
            hdc,
            [
                w::POINT::with(at - arrow, 0),
                w::POINT::with(at + arrow, 0),
                w::POINT::with(at, arrow),
            ],
            colours.playhead,
        );
        if focused {
            let _ = hdc.SetTextColor(colours.playhead);
            let _ = hdc.DrawFocusRect(rect(1, 1, client.right - 1, client.bottom - 1));
        }
    }
}

/// The keys the timeline takes from the dialog manager: the arrows and letters, and Escape when
/// it is the key pressed.
#[expect(unsafe_code, reason = "combines DLGC flags, which winsafe keeps apart")]
fn dialog_code(escape: bool) -> co::DLGC {
    let mut wanted = co::DLGC::WANTARROWS.raw() | co::DLGC::WANTCHARS.raw();
    if escape {
        wanted |= co::DLGC::WANTMESSAGE.raw();
    }
    // SAFETY: `DLGC` is a plain newtype over the flags, and these are its own.
    unsafe { co::DLGC::from_raw(wanted) }
}

fn key_of(key: co::VK) -> Option<Key> {
    let shifted = w::GetKeyState(co::VK::SHIFT).0;
    Some(match key {
        co::VK::SPACE => Key::PlayOrPause,
        co::VK::LEFT if shifted => Key::Jump(-1),
        co::VK::RIGHT if shifted => Key::Jump(1),
        co::VK::LEFT => Key::Step(-1),
        co::VK::RIGHT => Key::Step(1),
        co::VK::HOME => Key::First,
        co::VK::END => Key::Last,
        co::VK::CHAR_I => Key::MarkStart,
        co::VK::CHAR_O => Key::MarkEnd,
        co::VK::ESCAPE => Key::Leave,
        _ => return None,
    })
}

const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> w::RECT {
    w::RECT {
        left,
        top,
        right,
        bottom,
    }
}

fn fill(hdc: &w::HDC, area: w::RECT, colour: w::COLORREF) {
    let Ok(brush) = w::HBRUSH::GetStockObject(co::STOCK_BRUSH::DC) else {
        return;
    };
    let _ = hdc.SetDCBrushColor(colour);
    let _ = hdc.FillRect(area, &brush);
}

fn triangle(hdc: &w::HDC, points: [w::POINT; 3], colour: w::COLORREF) {
    let (Ok(pen), Ok(brush)) = (
        w::HPEN::GetStockObject(co::STOCK_PEN::DC),
        w::HBRUSH::GetStockObject(co::STOCK_BRUSH::DC),
    ) else {
        return;
    };
    let _ = hdc.SetDCPenColor(colour);
    let _ = hdc.SetDCBrushColor(colour);
    let (Ok(_pen), Ok(_brush)) = (hdc.SelectObject(&pen), hdc.SelectObject(&brush)) else {
        return;
    };
    let _ = hdc.Polygon(&points);
}
