//! Noticing full-screen apps. A window counts as full screen when its client area covers the
//! monitor it is on: that is how games present, whether exclusive or borderless, while a
//! maximized window never covers the taskbar. Windows reports foreground changes through an
//! event hook, delivered on the installing thread's message loop, so nothing polls.

use crate::{APP_NAME, service::Target};
use std::{cell::RefCell, rc::Rc};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{
            EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
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
pub(crate) struct ForegroundHook(HWINEVENTHOOK);

impl ForegroundHook {
    pub(crate) fn install(on_change: impl Fn() + 'static) -> Option<Self> {
        HANDLER.with(|handler| *handler.borrow_mut() = Some(Rc::new(on_change)));
        // SAFETY: an out-of-context hook for one event, with the callback below, which runs on
        // this thread's message loop; `APP_NAME` keeps the hook distinct from nothing.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(on_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            HANDLER.with(|handler| handler.borrow_mut().take());
            crate::log::warning(format!("{APP_NAME} could not watch the foreground window"));
            return None;
        }
        Some(Self(hook))
    }
}

impl Drop for ForegroundHook {
    fn drop(&mut self) {
        // SAFETY: unhooks the hook this value stands for.
        let _ = unsafe { UnhookWinEvent(self.0) };
        HANDLER.with(|handler| handler.borrow_mut().take());
    }
}

/// The handler is cloned out first, so it may itself replace or drop the hook.
unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _window: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    let handler = HANDLER.with(|handler| handler.borrow().clone());
    if let Some(handler) = handler {
        handler();
    }
}
