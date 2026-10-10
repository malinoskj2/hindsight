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

/// The foreground window when it is a full-screen app of another process, as a capture target.
pub(crate) fn fullscreen_app() -> Option<Target> {
    let window = w::HWND::GetForegroundWindow()?;
    let (_, process) = window.GetWindowThreadProcessId();
    let is_shell = [Some(w::HWND::GetDesktopWindow()), w::HWND::GetShellWindow()]
        .into_iter()
        .flatten()
        .any(|shell| shell == window);
    if is_shell
        || process == w::GetCurrentProcessId()
        || window.IsIconic()
        || !window.IsWindowVisible()
    {
        return None;
    }
    if let Ok(class) = window.GetClassName()
        && SHELL_CLASSES.contains(&class.as_str())
    {
        return None;
    }
    // A cloaked window is kept around but not shown: a suspended UWP app, a window on another
    // virtual desktop.
    if let Ok(w::DwmAttr::Cloaked(flags)) = window.DwmGetWindowAttribute(co::DWMWA::CLOAKED)
        && flags.raw() != 0
    {
        return None;
    }

    let monitor = window.MonitorFromWindow(co::MONITOR::DEFAULTTONEAREST);
    let bounds = monitor.GetMonitorInfo().ok()?.rcMonitor;
    let client = window.GetClientRect().ok()?;
    let origin = window.ClientToScreen(w::POINT::default()).ok()?;
    let covers = origin.x <= bounds.left
        && origin.y <= bounds.top
        && origin.x + client.right >= bounds.right
        && origin.y + client.bottom >= bounds.bottom;
    if !covers {
        return None;
    }

    Some(Target::Window {
        window: window.ptr() as isize,
        monitor: monitor.ptr() as isize,
        name: process_name(process),
    })
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

type Handler = Rc<dyn Fn()>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// Calls its handler, on the installing thread, whenever the foreground window changes.
/// Two hooks: one for foreground changes, and one for moves and resizes of the foreground
/// window. A game that goes full screen while already in front changes nothing else.
pub(crate) struct ForegroundHook(HWINEVENTHOOK, HWINEVENTHOOK);

impl ForegroundHook {
    pub(crate) fn install(on_change: impl Fn() + 'static) -> Option<Self> {
        HANDLER.with(|handler| *handler.borrow_mut() = Some(Rc::new(on_change)));
        let foreground = hook(EVENT_SYSTEM_FOREGROUND);
        let location = hook(EVENT_OBJECT_LOCATIONCHANGE);
        if foreground.is_invalid() || location.is_invalid() {
            // SAFETY: unhooks only the hooks that were set; an invalid one is skipped.
            unsafe {
                for valid in [foreground, location]
                    .into_iter()
                    .filter(|h| !h.is_invalid())
                {
                    let _ = UnhookWinEvent(valid);
                }
            }
            HANDLER.with(|handler| handler.borrow_mut().take());
            crate::log::warning(format!("{APP_NAME} could not watch the foreground window"));
            return None;
        }
        Some(Self(foreground, location))
    }
}

/// An out-of-context hook for one event, delivered on this thread's message loop.
fn hook(event: u32) -> HWINEVENTHOOK {
    // SAFETY: the callback is a plain function that only reads thread-local state; the hook
    // is removed when the `ForegroundHook` that owns it is dropped.
    unsafe {
        SetWinEventHook(
            event,
            event,
            None,
            Some(on_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        // SAFETY: unhooks the hooks this value stands for.
        unsafe {
            let _ = UnhookWinEvent(self.0);
            let _ = UnhookWinEvent(self.1);
        }
        HANDLER.with(|handler| handler.borrow_mut().take());
    }
}

/// The handler is cloned out first, so it may itself replace or drop the hook. A location
/// change counts only for the foreground window itself, not its controls or other windows.
unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_OBJECT_LOCATIONCHANGE {
        // SAFETY: reads the foreground window handle only.
        let foreground = unsafe { GetForegroundWindow() };
        if object != OBJID_WINDOW.0 || child != CHILDID_SELF.cast_signed() || window != foreground {
            return;
        }
    }
    let handler = HANDLER.with(|handler| handler.borrow().clone());
    if let Some(handler) = handler {
        handler();
    }
}
