//! Scheduling for the capture thread and the process: the thread wakes on time and ahead of a
//! game's threads, Windows does not slow the process down as background work, and the GPU
//! scheduler serves its small copies ahead of a game's rendering, the way `NvFBC`-based capture
//! runs inside the driver.

use windows::{
    Win32::{
        Foundation::{HANDLE, NTSTATUS},
        System::{
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Threading::{
                AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
                AvSetMmThreadPriority, GetCurrentProcess, GetCurrentThread,
                PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetProcessInformation, SetThreadPriority,
                THREAD_PRIORITY_HIGHEST,
            },
        },
    },
    core::{s, w},
};

/// `D3DKMT_SCHEDULINGPRIORITYCLASS` high.
const SCHEDULING_HIGH: i32 = 4;

type SetSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> NTSTATUS;

/// The calling thread's membership of the multimedia "Capture" scheduling class, which the
/// Multimedia Class Scheduler runs in the real-time priority range, above any game thread.
/// Reverted when dropped.
pub(super) struct MultimediaThread(HANDLE);

pub(super) fn join_multimedia_class() -> windows::core::Result<MultimediaThread> {
    let mut index = 0;
    // SAFETY: registers the calling thread with a built-in task name; the index is written.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &raw mut index) }?;
    let joined = MultimediaThread(handle);
    // SAFETY: the handle was just issued for this thread.
    unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) }?;
    Ok(joined)
}

impl Drop for MultimediaThread {
    fn drop(&mut self) {
        // SAFETY: reverts the registration this handle stands for.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// Raises the calling thread's CPU priority; the fallback when the multimedia class is
/// unavailable (its service is disabled).
pub(super) fn raise_thread() -> windows::core::Result<()> {
    // SAFETY: changes the priority of the calling thread only.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
}

/// Keeps Windows from running the process as efficiency-mode background work: no lowered clock
/// speed, and timer requests honoured while the window is hidden.
pub(super) fn opt_out_of_throttling() -> windows::core::Result<()> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    let size = u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>())
        .expect("the throttling state is a few bytes");
    // SAFETY: passes a correctly sized `PROCESS_POWER_THROTTLING_STATE` for the current process.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size,
        )
    }
}

/// Raises the process's GPU scheduling class to high, which needs no elevation. Realtime is
/// left alone: it could starve the game and the desktop compositor of the GPU. Reports whether
/// the class was set.
pub(super) fn raise_gpu_class() -> bool {
    // SAFETY: loads a system library by name; `gdi32.dll` is loaded in every GUI process.
    let Ok(gdi) = (unsafe { LoadLibraryExW(w!("gdi32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        return false;
    };
    // SAFETY: looks a symbol up in the loaded library.
    let Some(export) =
        (unsafe { GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) })
    else {
        return false;
    };
    // SAFETY: the export has this signature since Windows Vista.
    let set_class = unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetSchedulingPriorityClass>(
            export,
        )
    };
    // SAFETY: changes the scheduling class of the current process only.
    unsafe { set_class(GetCurrentProcess(), SCHEDULING_HIGH) }.is_ok()
}
