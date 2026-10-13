//! COM initialised on the calling thread for as long as the guard lives.

use windows::Win32::System::Com::{
    COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx,
    CoUninitialize,
};

pub(crate) struct Apartment;

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
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: matches the successful initialise that made `self`, on the same thread.
        unsafe { CoUninitialize() };
    }
}
