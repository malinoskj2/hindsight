//! OS-level helpers. Hotkey parsing, the icon and thread scheduling are portable in interface;
//! hotkey registration, full-screen app detection, the Start with Windows entry, the NvFBC key,
//! the tray icon, the window chrome and handing a second launch to the running copy are
//! Windows-only.

#[cfg(windows)]
#[expect(unsafe_code)]
pub(crate) mod foreground;
pub(crate) mod hotkey;
pub(crate) mod icon;
#[cfg(windows)]
pub(crate) mod instance;
#[cfg(windows)]
pub(crate) mod nvfbc_key;
pub(crate) mod sound;
#[cfg(windows)]
pub(crate) mod startup;
#[cfg_attr(windows, expect(unsafe_code))]
pub(crate) mod thread;
#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
pub(crate) mod window;
