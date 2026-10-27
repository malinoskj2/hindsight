//! Full-screen app detection: windows whose client area covers their monitor, watched through
//! event hooks.

use super::{
    callback::Slot,
    win32::{cb_size, cb_size_u16},
};
use crate::service::{MonitorHandle, Target, Through, WindowHandle};
use std::{path::Path, rc::Rc};
use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            DEVMODEW, DMDO_DEFAULT, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW,
            HMONITOR, MONITORINFOEXW,
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                CHILDID_SELF, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW,
                WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
            },
        },
    },
    core::PCWSTR,
};
use winsafe::{self as w, co};

/// Shell windows that fill the monitor: the desktop and its wallpaper hosts.
const SHELL_CLASSES: [&str; 2] = ["Progman", "WorkerW"];
/// Processes whose windows cover the monitor only as switchers and overlays, never as games: the
/// shell, the screenshot overlay, the Start menu, search and the lock screen.
const SHELL_PROCESSES: [&str; 6] = [
    "explorer.exe",
    "ScreenClippingHost.exe",
    "ShellExperienceHost.exe",
    "StartMenuExperienceHost.exe",
    "SearchHost.exe",
    "LockApp.exe",
];

thread_local! {
    static HANDLER: Slot<dyn Fn()> = const { Slot::new() };
}

/// Hooks on foreground and location changes. The location hook detects a game that goes full
/// screen while it is already the foreground window.
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
            crate::log::warning("Foreground hook · not installed · apps noticed every second");
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
        // SAFETY: an out-of-context hook takes no module handle, and the callback only reads
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
        // SAFETY: the hook is valid and owned by this value, and is unhooked only here.
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
    let size = covered_size(&window, &monitor)?;

    let name = process_name(process);
    if SHELL_PROCESSES
        .iter()
        .any(|shell| name.eq_ignore_ascii_case(shell))
    {
        return None;
    }
    Some(Target::Window {
        window: WindowHandle::new(window.ptr().addr().cast_signed())?,
        monitor: MonitorHandle::new(monitor.ptr().addr().cast_signed())?,
        name,
        title: window.GetWindowText().unwrap_or_default(),
        size,
        rotated: is_rotated(&monitor),
        through: Through::Window,
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

/// Whether the window exists but is not shown, such as a suspended UWP app or a window on another
/// virtual desktop.
fn is_cloaked(window: &w::HWND) -> bool {
    matches!(
        window.DwmGetWindowAttribute(co::DWMWA::CLOAKED),
        Ok(w::DwmAttr::Cloaked(flags)) if flags.raw() != 0
    )
}

/// The client size of a window whose client area covers `monitor`, or `None` when it does not.
fn covered_size(window: &w::HWND, monitor: &w::HMONITOR) -> Option<(u32, u32)> {
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
    Some((
        client.right.try_into().ok()?,
        client.bottom.try_into().ok()?,
    ))
}

/// Whether the monitor's desktop is turned from the panel's own orientation, as on a monitor
/// stood on its side. `false` when the orientation cannot be read.
fn is_rotated(monitor: &w::HMONITOR) -> bool {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = cb_size::<MONITORINFOEXW>();
    let mut mode = DEVMODEW {
        dmSize: cb_size_u16::<DEVMODEW>(),
        ..Default::default()
    };
    // SAFETY: `info` is a MONITORINFOEXW and its size field says so.
    if !unsafe { GetMonitorInfoW(HMONITOR(monitor.ptr()), (&raw mut info).cast()) }.as_bool() {
        return false;
    }
    let device = PCWSTR(info.szDevice.as_ptr());
    // SAFETY: the device name is null-terminated; `mode` is a DEVMODEW and its size field says
    // so.
    if !unsafe { EnumDisplaySettingsW(device, ENUM_CURRENT_SETTINGS, &raw mut mode) }.as_bool() {
        return false;
    }
    // SAFETY: the display branch of the union is the one EnumDisplaySettingsW fills.
    unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation != DMDO_DEFAULT }
}

fn process_name(process: u32) -> String {
    w::HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, process)
        .and_then(|handle| handle.QueryFullProcessImageName(co::PROCESS_NAME::WIN32))
        .ok()
        .and_then(|path| {
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "a full-screen app".to_owned())
}

extern "system" fn on_event(
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
    let foreground =
        w::HWND::GetForegroundWindow().map_or(HWND::default(), |found| HWND(found.ptr()));
    object == OBJID_WINDOW.0 && child == CHILDID_SELF.cast_signed() && window == foreground
}
