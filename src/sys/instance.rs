//! One copy at a time: a later launch hands itself to the running copy's window and exits.

use std::{thread, time::Duration};
use winsafe::{self as w, co};

pub(crate) const WINDOW_CLASS: &str = "HindsightMainWindow";
const WAIT: Duration = Duration::from_millis(100);
const ATTEMPTS: u32 = 50;

#[expect(unsafe_code)]
// SAFETY: `WM` is a plain newtype over the message number; this one is in the app range.
pub(crate) const SHOW_MESSAGE: co::WM = unsafe { co::WM::from_raw(co::WM::APP.raw() + 1) };

pub(crate) fn show_running() -> bool {
    for _ in 0..ATTEMPTS {
        if let Ok(Some(window)) =
            w::HWND::FindWindow(Some(w::AtomStr::from_str(WINDOW_CLASS)), None)
            && bring_forward(&window)
        {
            return true;
        }
        thread::sleep(WAIT);
    }
    false
}

#[expect(unsafe_code)]
fn bring_forward(window: &w::HWND) -> bool {
    // This process has the focus, so it may hand it to the other copy.
    let (_, process) = window.GetWindowThreadProcessId();
    let _ = w::AllowSetForegroundWindow(Some(process));
    // SAFETY: the message carries no pointers.
    unsafe { window.PostMessage(w::msg::Wm::new(SHOW_MESSAGE, 0, 0)) }.is_ok()
}
