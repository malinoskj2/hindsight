//! OS-level helpers, one Win32 facility each.

#[cfg(windows)]
pub(crate) mod callback;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls QueryPerformanceCounter")]
pub(crate) mod clock;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls CoInitializeEx and SHGetKnownFolderPath")]
pub(crate) mod com;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls SetWinEventHook and reads other windows")]
pub(crate) mod foreground;
pub(crate) mod hotkey;
#[cfg(windows)]
pub(crate) mod icon;
#[cfg(windows)]
pub(crate) mod instance;
#[cfg(windows)]
pub(crate) mod release;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls the shell and GDI")]
pub(crate) mod shell;
pub(crate) mod sound;
#[cfg(windows)]
pub(crate) mod startup;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls the thread scheduling API")]
pub(crate) mod thread;
#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls GetDiskFreeSpaceExW")]
pub(crate) mod volume;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls DWM and sends raw window messages")]
pub(crate) mod window;
