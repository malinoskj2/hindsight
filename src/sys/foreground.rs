//! Noticing full-screen apps. A window counts as full screen when its client area covers the
//! monitor it is on: that is how games present, whether exclusive or borderless, while a
//! maximized window never covers the taskbar. Windows reports foreground changes, and moves and
//! resizes of the foreground window, through event hooks delivered on the installing thread's
//! message loop, so nothing polls.

use crate::{APP_NAME, service::Target};
use std::{cell::RefCell, rc::Rc};
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

/// Windows of the shell that fill the monitor without being apps: the desktop and its
/// wallpaper hosts.
const SHELL_CLASSES: [&str; 2] = ["Progman", "WorkerW"];

/// The callback of the installed hook.
type Handler = Rc<dyn Fn()>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// Calls its handler, on the installing thread, whenever the foreground window changes.
/// Two hooks: one for foreground changes, and one for moves and resizes of the foreground
/// window. A game that goes full screen while already in front changes nothing else.
pub(crate) struct ForegroundHook {
    _foreground: EventHook,
    _location: EventHook,
}

impl ForegroundHook {
    pub(crate) fn install(on_change: impl Fn() + 'static) -> Option<Self> {
        set_handler(Rc::new(on_change));
        // A hook that was set is removed again when the other one fails.
        let (Some(foreground), Some(location)) = (
            EventHook::set(EVENT_SYSTEM_FOREGROUND),
            EventHook::set(EVENT_OBJECT_LOCATIONCHANGE),
        ) else {
            clear_handler();
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
        // The hooks themselves are removed when the fields drop, right after this.
        clear_handler();
    }
}

/// An out-of-context hook for one event, delivered on this thread's message loop and removed
/// when dropped.
struct EventHook(HWINEVENTHOOK);

impl EventHook {
    fn set(event: u32) -> Option<Self> {
        // SAFETY: no module handle is passed, as an out-of-context hook requires, and the
        // callback is a plain function with the `WINEVENTPROC` signature that only reads
        // thread-local state. The hook is removed when the returned value is dropped.
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
        // SAFETY: the handle is a valid hook this value owns, and it is unhooked only here.
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

/// The foreground window when it is a full-screen app of another process, as a capture target.
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

/// Whether the window is the desktop or the shell's: the desktop window, the shell window, or
/// one of the classes in `SHELL_CLASSES`.
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

/// Whether the window is cloaked: kept around but not shown, like a suspended UWP app or a
/// window on another virtual desktop.
fn is_cloaked(window: &w::HWND) -> bool {
    matches!(
        window.DwmGetWindowAttribute(co::DWMWA::CLOAKED),
        Ok(w::DwmAttr::Cloaked(flags)) if flags.raw() != 0
    )
}

/// Whether the window's client area covers the whole of `monitor`; `None` when the geometry of
/// either cannot be read.
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

/// The executable's file name, or a stand-in when the process cannot be opened.
fn process_name(process: u32) -> String {
    w::HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, process)
        .and_then(|handle| handle.QueryFullProcessImageName(co::PROCESS_NAME::WIN32))
        .ok()
        .and_then(|path| path.rsplit('\\').next().map(str::to_owned))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "a full-screen app".to_owned())
}

/// The hooks' callback. A location change counts only for the foreground window itself, not
/// its controls or other windows.
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
    dispatch();
}

/// Whether an event's object is the foreground window itself, rather than one of its controls
/// or another window.
fn is_foreground_window(window: HWND, object: i32, child: i32) -> bool {
    // SAFETY: `GetForegroundWindow` takes no arguments and has no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    object == OBJID_WINDOW.0 && child == CHILDID_SELF.cast_signed() && window == foreground
}

/// Makes `handler` this thread's callback, replacing any other.
fn set_handler(handler: Handler) {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(handler));
}

/// Forgets this thread's callback.
fn clear_handler() {
    HANDLER.with(|slot| slot.borrow_mut().take());
}

/// Calls this thread's callback, if there is one. It is cloned out first, so it may itself
/// replace or drop the hook.
fn dispatch() {
    let handler = HANDLER.with(|slot| slot.borrow().clone());
    if let Some(handler) = handler {
        handler();
    }
}
