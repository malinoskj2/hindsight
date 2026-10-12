//! Scheduling of the application's own helper threads.

use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(windows)]
use windows::Win32::System::{
    Kernel::PROCESSOR_NUMBER,
    Threading::{
        GetCurrentThread, GetThreadIdealProcessorEx, SetThreadIdealProcessorEx, SetThreadPriority,
        THREAD_MODE_BACKGROUND_BEGIN,
    },
};

/// Logical processors per Windows processor group.
const GROUP_SIZE: u32 = 64;
static CAPTURE_PROCESSOR: AtomicU32 = AtomicU32::new(u32::MAX);

pub(crate) fn note_capture_thread() {
    if let Some(processor) = ideal_processor() {
        CAPTURE_PROCESSOR.store(processor, Ordering::Relaxed);
    }
}

/// Hints the scheduler to run the caller half the machine away from the capture thread, so they
/// share neither a core nor a cache complex; `false` until the capture thread has published.
pub(crate) fn keep_off_capture_core() -> bool {
    let capture = CAPTURE_PROCESSOR.load(Ordering::Relaxed);
    if capture == u32::MAX {
        return false;
    }
    let count = std::thread::available_parallelism()
        .map_or(1, |count| u32::try_from(count.get()).unwrap_or(u32::MAX));
    if count < 4 {
        return true;
    }
    prefer_processor((capture + count / 2) % count);
    true
}

#[cfg(windows)]
fn ideal_processor() -> Option<u32> {
    let mut number = PROCESSOR_NUMBER::default();
    // SAFETY: the pseudo-handle names the calling thread; the out-parameter is a live local.
    unsafe { GetThreadIdealProcessorEx(GetCurrentThread(), &raw mut number) }.ok()?;
    Some(u32::from(number.Group) * GROUP_SIZE + u32::from(number.Number))
}

#[cfg(windows)]
fn prefer_processor(processor: u32) {
    let number = PROCESSOR_NUMBER {
        Group: u16::try_from(processor / GROUP_SIZE).unwrap_or(u16::MAX),
        Number: u8::try_from(processor % GROUP_SIZE).unwrap_or(u8::MAX),
        Reserved: 0,
    };
    // SAFETY: the pseudo-handle names the calling thread; a refused hint changes nothing.
    let _ = unsafe { SetThreadIdealProcessorEx(GetCurrentThread(), &raw const number, None) };
}

#[cfg(not(windows))]
fn ideal_processor() -> Option<u32> {
    None
}

#[cfg(not(windows))]
fn prefer_processor(_processor: u32) {}

/// Lowers the thread's CPU, I/O and memory priority, so a clip write does not compete with a game.
#[cfg(windows)]
pub(crate) fn enter_background_mode() -> windows::core::Result<()> {
    // SAFETY: `GetCurrentThread` is a pseudo-handle, always valid and never closed.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }
}

#[cfg(not(windows))]
#[expect(clippy::unnecessary_wraps)]
pub(crate) fn enter_background_mode() -> Result<(), std::convert::Infallible> {
    Ok(())
}
