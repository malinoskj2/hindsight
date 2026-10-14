//! Scheduling of the application's own helper threads.

use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Kernel::PROCESSOR_NUMBER,
        Threading::{
            GetThreadIdealProcessorEx, SetThreadIdealProcessorEx, SetThreadPriority,
            THREAD_MODE_BACKGROUND_BEGIN,
        },
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
pub(crate) fn ideal_processor() -> Option<u32> {
    let mut number = PROCESSOR_NUMBER::default();
    // SAFETY: the pseudo-handle names the calling thread; the out-parameter is a live local.
    unsafe { GetThreadIdealProcessorEx(current_thread(), &raw mut number) }.ok()?;
    Some(u32::from(number.Group) * GROUP_SIZE + u32::from(number.Number))
}

fn prefer_processor(processor: u32) {
    let number = PROCESSOR_NUMBER {
        Group: u16::try_from(processor / GROUP_SIZE).unwrap_or(u16::MAX),
        Number: u8::try_from(processor % GROUP_SIZE).unwrap_or(u8::MAX),
        Reserved: 0,
    };
    // SAFETY: the pseudo-handle names the calling thread; a refused hint changes nothing.
    let _ = unsafe { SetThreadIdealProcessorEx(current_thread(), &raw const number, None) };
}

/// Lowers the thread's CPU, I/O and memory priority, so a clip write does not compete with a game.
pub(crate) fn enter_background_mode() -> windows::core::Result<()> {
    // SAFETY: the pseudo-handle names the calling thread, always valid and never closed.
    unsafe { SetThreadPriority(current_thread(), THREAD_MODE_BACKGROUND_BEGIN) }
}

/// The calling thread's pseudo-handle, which needs no closing.
fn current_thread() -> HANDLE {
    HANDLE(winsafe::HTHREAD::GetCurrentThread().ptr())
}
