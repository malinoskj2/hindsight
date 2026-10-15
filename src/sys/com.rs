//! A guard that keeps COM initialised on the calling thread for as long as it lives, and the
//! shell's known folders.

use std::{marker::PhantomData, path::PathBuf};
use winsafe::{self as w, co, guard::CoUninitializeGuard};

/// A COM apartment on the calling thread. It is neither `Send` nor `Sync`, because
/// `CoUninitialize` balances the initialisation of the thread it runs on, so the guard must be
/// dropped on the thread that created it.
pub(crate) struct Apartment {
    _guard: CoUninitializeGuard,
    _thread: PhantomData<*const ()>,
}

impl Apartment {
    /// Enters a single-threaded apartment, which the shell's thumbnail providers expect.
    pub(crate) fn single_threaded() -> windows::core::Result<Self> {
        Self::start(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE)
    }

    /// Enters the multithreaded apartment, which the audio and Media Foundation objects belong to.
    pub(crate) fn multi_threaded() -> windows::core::Result<Self> {
        Self::start(co::COINIT::MULTITHREADED)
    }

    fn start(flags: co::COINIT) -> windows::core::Result<Self> {
        // Every success, including "already initialised", is balanced by the guard's uninitialise.
        let guard = w::CoInitializeEx(flags).map_err(hresult_error)?;
        // winsafe treats a thread already in the other apartment as started, but that call
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

/// Converts a winsafe COM error into the `windows` crate's error type, which the callers report.
pub(crate) fn hresult_error(code: co::HRESULT) -> windows::core::Error {
    windows::core::HRESULT(code.raw().cast_signed()).into()
}

/// Returns the current user's known folder `id`, such as `co::KNOWNFOLDERID::Videos`, at its
/// current location if it has been moved. Returns `None` when the shell cannot resolve it.
pub(crate) fn known_folder(id: &co::KNOWNFOLDERID) -> Option<PathBuf> {
    // A null token selects the calling user's folder. The call needs no COM apartment.
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
