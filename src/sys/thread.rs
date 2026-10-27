//! Scheduling of the application's own helper threads.

use super::win32::cb_size;
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Kernel::PROCESSOR_NUMBER,
        Threading::{
            GetThreadIdealProcessorEx, SetThreadIdealProcessorEx, SetThreadInformation,
            SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            THREAD_POWER_THROTTLING_STATE, ThreadPowerThrottling,
        },
    },
};

/// Logical processors per Windows processor group.
const GROUP_SIZE: u32 = 64;

/// Hints to the scheduler that the calling thread should run half the processor count away from
/// the thread on processor `other`, so that they share neither a core nor a cache complex.
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
    // SAFETY: the pseudo-handle names the calling thread, and the out-parameter is a live local.
    unsafe { GetThreadIdealProcessorEx(current_thread(), &raw mut number) }.ok()?;
    Some(u32::from(number.Group) * GROUP_SIZE + u32::from(number.Number))
}

fn prefer_processor(processor: u32) {
    let number = PROCESSOR_NUMBER {
        Group: u16::try_from(processor / GROUP_SIZE).unwrap_or(u16::MAX),
        Number: u8::try_from(processor % GROUP_SIZE).unwrap_or(u8::MAX),
        Reserved: 0,
    };
    // SAFETY: the pseudo-handle names the calling thread, and a refused hint changes nothing.
    let _ = unsafe { SetThreadIdealProcessorEx(current_thread(), &raw const number, None) };
}

/// Lowers the thread's CPU, I/O and memory priority, so a clip write does not compete with a game.
pub(crate) fn enter_background_mode() -> windows::core::Result<()> {
    // SAFETY: the pseudo-handle names the calling thread, always valid and never closed.
    unsafe { SetThreadPriority(current_thread(), THREAD_MODE_BACKGROUND_BEGIN) }
}

/// Tags the calling thread EcoQoS, so that Windows runs it on efficiency cores and at low clocks:
/// slower, but lighter on the processor the game shares. The tag is the thread's own, apart from
/// the process's opt-out of throttling for capture.
pub(crate) fn prefer_efficiency() -> windows::core::Result<()> {
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: the pseudo-handle names the calling thread, and `state` is a
    // `THREAD_POWER_THROTTLING_STATE` of the size passed, live for the call.
    unsafe {
        SetThreadInformation(
            current_thread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            cb_size::<THREAD_POWER_THROTTLING_STATE>(),
        )
    }
}

/// The calling thread's pseudo-handle, which need not be closed.
fn current_thread() -> HANDLE {
    HANDLE(winsafe::HTHREAD::GetCurrentThread().ptr())
}
