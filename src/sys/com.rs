//! COM initialised on the calling thread for as long as the guard lives, and the shell's known
//! folders, which it hands back in COM task memory.

use std::{ffi::OsString, marker::PhantomData, os::windows::ffi::OsStringExt, path::PathBuf};
use windows::{
    Win32::{
        System::Com::{
            COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
            CoInitializeEx, CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    },
    core::GUID,
};

/// Neither `Send` nor `Sync`: `CoUninitialize` balances the initialise of the thread it runs on,
/// so the guard must drop on the thread that made it.
pub(crate) struct Apartment(PhantomData<*const ()>);

impl Apartment {
    /// A single-threaded apartment, which the shell's thumbnail providers expect.
    pub(crate) fn single_threaded() -> windows::core::Result<Self> {
        Self::start(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
    }

    /// The multithreaded apartment, where the audio and Media Foundation objects live.
    pub(crate) fn multi_threaded() -> windows::core::Result<Self> {
        Self::start(COINIT_MULTITHREADED)
    }

    fn start(flags: COINIT) -> windows::core::Result<Self> {
        // SAFETY: every success, including "already initialised", is matched by the
        // uninitialise in `drop`.
        unsafe { CoInitializeEx(None, flags) }.ok()?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: matches the successful initialise that made `self`; the guard is not `Send`,
        // so this is the thread that initialised.
        unsafe { CoUninitialize() };
    }
}

/// The current user's known folder `id`, such as `FOLDERID_Videos`, wherever it was moved to;
/// `None` when the shell cannot resolve it.
pub(crate) fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` points to a live GUID for the duration of the call, and no token asks for
    // the calling user's folder; the call needs no COM apartment.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: on success `path` is a NUL-terminated string the shell allocated, still alive
    // because it is freed only below.
    let folder = PathBuf::from(OsString::from_wide(unsafe { path.as_wide() }));
    // SAFETY: the shell allocated `path` with the COM task allocator and nothing else owns it;
    // it is freed exactly once and not read again.
    unsafe { CoTaskMemFree(Some(path.0.cast_const().cast())) };
    Some(folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_Videos};

    #[test]
    fn known_folders_resolve_to_absolute_paths() {
        for id in [FOLDERID_LocalAppData, FOLDERID_Videos] {
            assert!(known_folder(&id).unwrap().is_absolute());
        }
    }
}
