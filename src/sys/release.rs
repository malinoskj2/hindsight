//! The Windows release, as the registry's `CurrentVersion` key records it.

use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Each part is `None` when the registry lacks it; Windows 10 before 20H2 has no display version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Release {
    /// `25H2`.
    pub(crate) display_version: Option<String>,
    pub(crate) build: Option<u32>,
    /// The update revision, `6899` in build `26200.6899`.
    pub(crate) revision: Option<u32>,
}

pub(crate) fn current() -> Release {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(CURRENT_VERSION) else {
        return Release::default();
    };
    Release {
        display_version: key.get_value::<String, _>("DisplayVersion").ok(),
        build: key
            .get_value::<String, _>("CurrentBuild")
            .ok()
            .and_then(|build| build.trim().parse().ok()),
        revision: key.get_value::<u32, _>("UBR").ok(),
    }
}
