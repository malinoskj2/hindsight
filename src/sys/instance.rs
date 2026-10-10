//! One copy at a time. The first copy holds the state lock and shows the window; a later
//! launch, from the Start menu, the Run key, a shortcut or a terminal, finds that window by
//! its class, asks it to come forward and exits, so there is never a second recorder.

use std::{thread, time::Duration};
use winsafe::{self as w, co};

/// The window class of the application window, which a later launch looks for.
pub(crate) const WINDOW_CLASS: &str = "HindsightMainWindow";

/// The first message of the application's own range: "come forward".
#[expect(unsafe_code)]
pub(crate) fn show_message() -> co::WM {
    // SAFETY: every number is a valid window message identifier, and this one is in the range
    // Windows leaves to the application.
    unsafe { co::WM::from_raw(co::WM::APP.raw() + 1) }
}

/// How often, and for how long, to look for the window: the first copy may still be starting.
const WAIT: Duration = Duration::from_millis(100);
const ATTEMPTS: u32 = 50;

/// Asks the running copy's window to come forward, restoring it from the tray or the taskbar.
/// Reports whether there was a window to ask.
pub(crate) fn show_running() -> bool {
    for _ in 0..ATTEMPTS {
        if let Ok(Some(window)) =
            w::HWND::FindWindow(Some(w::AtomStr::from_str(WINDOW_CLASS)), None)
        {
            // This launch was started by the user, so it may let the other copy take the focus.
            let (_, process) = window.GetWindowThreadProcessId();
            let _ = w::AllowSetForegroundWindow(Some(process));
            if post(&window) {
                return true;
            }
        }
        thread::sleep(WAIT);
    }
    false
}

#[expect(unsafe_code)]
fn post(window: &w::HWND) -> bool {
    // SAFETY: the message carries no pointers, and the window answers it from its own state.
    unsafe { window.PostMessage(w::msg::Wm::new(show_message(), 0, 0)) }.is_ok()
}
