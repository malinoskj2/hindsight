//! Scheduling of the application's own helper threads.

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

/// Hints the scheduler to run the caller half the machine away from the thread on processor
/// `other`, so they share neither a core nor a cache complex.
pub(crate) fn keep_off(other: u32) {
    let count = std::thread::available_parallelism()
        .map_or(1, |count| u32::try_from(count.get()).unwrap_or(u32::MAX));
    if count < 4 {
        return;
    }
    prefer_processor((other + count / 2) % count);
}

/// The logical processor the scheduler prefers for the calling thread.
#[cfg(windows)]
pub(crate) fn ideal_processor() -> Option<u32> {
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
pub(crate) fn ideal_processor() -> Option<u32> {
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
