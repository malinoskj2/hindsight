//! Full-screen apps: windows whose client area covers their monitor, watched through event hooks.

use super::callback::Slot;
use crate::{APP_NAME, service::Target};
use std::rc::Rc;
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            CHILDID_SELF, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
            GetForegroundWindow, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
    },
};
use winsafe::{self as w, co};

/// Shell windows that fill the monitor: the desktop and its wallpaper hosts.
const SHELL_CLASSES: [&str; 2] = ["Progman", "WorkerW"];

thread_local! {
    static HANDLER: Slot<dyn Fn()> = const { Slot::new() };
}

/// The location hook catches a game going full screen while already in front.
pub(crate) struct ForegroundHook {
    _foreground: EventHook,
    _location: EventHook,
}

impl ForegroundHook {
    pub(crate) fn install(on_change: impl Fn() + 'static) -> Option<Self> {
        HANDLER.with(|slot| slot.set(Rc::new(on_change)));
        let (Some(foreground), Some(location)) = (
            EventHook::set(EVENT_SYSTEM_FOREGROUND),
            EventHook::set(EVENT_OBJECT_LOCATIONCHANGE),
        ) else {
            HANDLER.with(Slot::clear);
            crate::log::warning(format!("{APP_NAME} could not watch the foreground window"));
            return None;
        };
        Some(Self {
            _foreground: foreground,
            _location: location,
        })
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        HANDLER.with(Slot::clear);
    }
}

struct EventHook(HWINEVENTHOOK);

impl EventHook {
    fn set(event: u32) -> Option<Self> {
        // SAFETY: an out-of-context hook takes no module handle; the callback only reads
        // thread-local state.
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(on_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        (!hook.is_invalid()).then_some(Self(hook))
    }
}

impl Drop for EventHook {
    fn drop(&mut self) {
        // SAFETY: a valid hook this value owns, unhooked only here.
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

pub(crate) fn fullscreen_app() -> Option<Target> {
    let window = w::HWND::GetForegroundWindow()?;
    let (_, process) = window.GetWindowThreadProcessId();
    if is_shell(&window)
        || process == w::GetCurrentProcessId()
        || window.IsIconic()
        || !window.IsWindowVisible()
        || is_cloaked(&window)
    {
        return None;
    }

    let monitor = window.MonitorFromWindow(co::MONITOR::DEFAULTTONEAREST);
    if !covers(&window, &monitor)? {
        return None;
    }

    Some(Target::Window {
        window: window.ptr() as isize,
        monitor: monitor.ptr() as isize,
        name: process_name(process),
    })
}

fn is_shell(window: &w::HWND) -> bool {
    let is_shell_window = [Some(w::HWND::GetDesktopWindow()), w::HWND::GetShellWindow()]
        .into_iter()
        .flatten()
        .any(|shell| shell == *window);
    is_shell_window
        || window
            .GetClassName()
            .is_ok_and(|class| SHELL_CLASSES.contains(&class.as_str()))
}

/// Kept around but not shown: a suspended UWP app, or a window on another virtual desktop.
fn is_cloaked(window: &w::HWND) -> bool {
    matches!(
        window.DwmGetWindowAttribute(co::DWMWA::CLOAKED),
        Ok(w::DwmAttr::Cloaked(flags)) if flags.raw() != 0
    )
}

fn covers(window: &w::HWND, monitor: &w::HMONITOR) -> Option<bool> {
    let bounds = monitor.GetMonitorInfo().ok()?.rcMonitor;
    let client = window.GetClientRect().ok()?;
    let origin = window.ClientToScreen(w::POINT::default()).ok()?;
    Some(
        origin.x <= bounds.left
            && origin.y <= bounds.top
            && origin.x + client.right >= bounds.right
            && origin.y + client.bottom >= bounds.bottom,
    )
}

fn process_name(process: u32) -> String {
    w::HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, process)
        .and_then(|handle| handle.QueryFullProcessImageName(co::PROCESS_NAME::WIN32))
        .ok()
        .and_then(|path| path.rsplit('\\').next().map(str::to_owned))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "a full-screen app".to_owned())
}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_OBJECT_LOCATIONCHANGE && !is_foreground_window(window, object, child) {
        return;
    }
    if let Some(handler) = HANDLER.with(Slot::get) {
        handler();
    }
}

fn is_foreground_window(window: HWND, object: i32, child: i32) -> bool {
    // SAFETY: no arguments, no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    object == OBJID_WINDOW.0 && child == CHILDID_SELF.cast_signed() && window == foreground
}
