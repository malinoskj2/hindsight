//! OS-level helpers. Hotkey parsing and the icon are portable; hotkey registration, the Start
//! with Windows entry, the tray icon and the window chrome are Windows-only.

pub(crate) mod hotkey;
pub(crate) mod icon;
#[cfg(windows)]
pub(crate) mod startup;
#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
pub(crate) mod window;
