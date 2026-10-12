//! OS-level helpers, one Win32 facility each. Hotkey parsing, the icon, the save sounds and
//! thread scheduling are portable in interface; the performance-counter clock, hotkey
//! registration, full-screen app detection, the Start with Windows entry, the NvFBC key, the
//! shell's thumbnails, file opening, showing in Explorer and recycling, the tray icon, the
//! window chrome and handing a second launch to the running copy are Windows-only.

#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod clock;
#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod foreground;
pub(crate) mod hotkey;
pub(crate) mod icon;
#[cfg(windows)]
pub(crate) mod instance;
#[cfg(windows)]
pub(crate) mod nvfbc_key;
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
pub(crate) mod window;
