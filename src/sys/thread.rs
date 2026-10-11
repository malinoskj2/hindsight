//! Scheduling of the application's own helper threads: the writer's background mode, and
//! keeping the audio thread away from the capture thread's core.

use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(windows)]
use windows::Win32::System::{
    Kernel::PROCESSOR_NUMBER,
    Threading::{
        GetCurrentThread, GetThreadIdealProcessorEx, SetThreadIdealProcessorEx, SetThreadPriority,
        THREAD_MODE_BACKGROUND_BEGIN,
    },
};

/// Logical processors in a processor group, as Windows numbers them.
const GROUP_SIZE: u32 = 64;
/// The processor the capture thread prefers, published by that thread; `u32::MAX` until it has.
static CAPTURE_PROCESSOR: AtomicU32 = AtomicU32::new(u32::MAX);

/// Records the calling thread as the capture thread, so other threads can keep off its core.
pub(crate) fn note_capture_thread() {
    if let Some(processor) = ideal_processor() {
        CAPTURE_PROCESSOR.store(processor, Ordering::Relaxed);
    }
}

/// Points the calling thread at the processor half the machine away from the capture thread's,
/// so the two never share a core's execution units or, on processors made of several core
/// complexes, a cache: measured, an audio thread next to the capture thread cost the capture
/// thread about 30 Mcycles/s of its own time. A hint to the scheduler, not a binding. Returns
/// `false` while the capture thread has not published its processor yet, so the caller tries
/// again later; `true` once the hint is given or the machine is too small for it to matter.
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

/// The processor the scheduler prefers to run the calling thread on.
#[cfg(windows)]
fn ideal_processor() -> Option<u32> {
    let mut number = PROCESSOR_NUMBER::default();
    // SAFETY: the pseudo-handle names the calling thread; the out-parameter is written on
    // success.
    unsafe { GetThreadIdealProcessorEx(GetCurrentThread(), &raw mut number) }.ok()?;
    Some(u32::from(number.Group) * GROUP_SIZE + u32::from(number.Number))
}

/// Asks the scheduler to prefer `processor` for the calling thread.
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

/// Elsewhere the scheduler is left to itself.
#[cfg(not(windows))]
fn ideal_processor() -> Option<u32> {
    None
}

#[cfg(not(windows))]
fn prefer_processor(_processor: u32) {}

/// Marks the calling thread as background work: Windows lowers its CPU, I/O and memory
/// priority, so a clip being written does not compete with a game for the disk or the cores.
#[cfg(windows)]
pub(crate) fn enter_background_mode() -> windows::core::Result<()> {
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
