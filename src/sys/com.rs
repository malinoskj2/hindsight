//! COM initialised on the calling thread for as long as the guard lives.

use std::marker::PhantomData;
use windows::Win32::System::Com::{
    COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx,
    CoUninitialize,
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
