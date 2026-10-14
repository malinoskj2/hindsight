//! COM initialised on the calling thread for as long as the guard lives, and the shell's known
//! folders.

use std::{marker::PhantomData, path::PathBuf};
use winsafe::{self as w, co, guard::CoUninitializeGuard};

/// Neither `Send` nor `Sync`: `CoUninitialize` balances the initialise of the thread it runs on,
/// so the guard must drop on the thread that made it.
pub(crate) struct Apartment {
    _guard: CoUninitializeGuard,
    _thread: PhantomData<*const ()>,
}

impl Apartment {
    /// A single-threaded apartment, which the shell's thumbnail providers expect.
    pub(crate) fn single_threaded() -> windows::core::Result<Self> {
        Self::start(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE)
    }

    /// The multithreaded apartment, where the audio and Media Foundation objects live.
    pub(crate) fn multi_threaded() -> windows::core::Result<Self> {
        Self::start(co::COINIT::MULTITHREADED)
    }

    fn start(flags: co::COINIT) -> windows::core::Result<Self> {
        // Every success, including "already initialised", is matched by the guard's uninitialise.
        let guard = w::CoInitializeEx(flags).map_err(hresult_error)?;
        // winsafe counts a thread already in the other apartment as started, but that call
        // failed and must not be balanced.
        if guard.hr() == co::HRESULT::RPC_E_CHANGED_MODE {
            std::mem::forget(guard);
            return Err(hresult_error(co::HRESULT::RPC_E_CHANGED_MODE));
        }
        Ok(Self {
            _guard: guard,
            _thread: PhantomData,
        })
    }
}

/// A winsafe COM error as the `windows` crate's, which the callers report.
pub(crate) fn hresult_error(code: co::HRESULT) -> windows::core::Error {
    windows::core::HRESULT(code.raw().cast_signed()).into()
}

/// The current user's known folder `id`, such as `co::KNOWNFOLDERID::Videos`, wherever it was
/// moved to; `None` when the shell cannot resolve it.
pub(crate) fn known_folder(id: &co::KNOWNFOLDERID) -> Option<PathBuf> {
    // No token asks for the calling user's folder; the call needs no COM apartment.
    w::SHGetKnownFolderPath(id, co::KF::DEFAULT, None)
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_resolve_to_absolute_paths() {
        for id in [co::KNOWNFOLDERID::LocalAppData, co::KNOWNFOLDERID::Videos] {
            assert!(known_folder(&id).unwrap().is_absolute());
        }
    }
}
