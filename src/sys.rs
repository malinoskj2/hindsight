//! OS-level helpers, one Win32 facility each.

#[cfg(windows)]
pub(crate) mod callback;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod clock;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod com;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod foreground;
pub(crate) mod hotkey;
#[cfg(windows)]
pub(crate) mod icon;
#[cfg(windows)]
pub(crate) mod instance;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod shell;
pub(crate) mod sound;
#[cfg(windows)]
pub(crate) mod startup;
#[cfg_attr(windows, expect(unsafe_code))]
pub(crate) mod thread;
#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod volume;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod window;
