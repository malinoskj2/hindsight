//! Scheduling of the application's own helper threads.

/// Marks the calling thread as background work: Windows lowers its CPU, I/O and memory
/// priority, so a clip being written does not compete with a game for the disk or the cores.
#[cfg(windows)]
pub(crate) fn enter_background_mode() -> windows::core::Result<()> {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: `GetCurrentThread` returns a pseudo-handle to the calling thread that is always
    // valid and never closed; the call changes that thread's mode only.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }
}

/// Only Windows has a background mode; elsewhere the thread runs as it is. The signature
/// matches the Windows one so the caller reads the same on both.
#[cfg(not(windows))]
#[expect(clippy::unnecessary_wraps)]
pub(crate) fn enter_background_mode() -> Result<(), std::convert::Infallible> {
    Ok(())
}
