//! Scheduling for the capture thread and the process: the thread wakes on time, Windows does not
//! slow the process down as background work, and the GPU scheduler serves its small copies
//! ahead of a game's rendering, the way `NvFBC`-based capture runs inside the driver.

use windows::{
    Win32::{
        Foundation::{HANDLE, NTSTATUS},
        System::{
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Threading::{
                GetCurrentProcess, GetCurrentThread, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
                ProcessPowerThrottling, SetProcessInformation, SetThreadPriority,
                THREAD_PRIORITY_HIGHEST,
            },
        },
    },
    core::{s, w},
};

/// `D3DKMT_SCHEDULINGPRIORITYCLASS`: realtime needs an elevated process; high does not.
const SCHEDULING_REALTIME: i32 = 5;
const SCHEDULING_HIGH: i32 = 4;

type SetSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> NTSTATUS;

/// Raises the calling thread's CPU priority.
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
    // SAFETY: passes a correctly sized `PROCESS_POWER_THROTTLING_STATE` for the current process.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>()).unwrap_or(u32::MAX),
        )
    }
}

/// Raises the process's GPU scheduling class: realtime when elevated, else high. Returns the
/// class it got, by name.
pub(super) fn raise_gpu_class() -> Option<&'static str> {
    // SAFETY: `gdi32.dll` is loaded in every GUI process; the export has this signature since
    // Windows Vista.
    let set_class = unsafe {
        let gdi = LoadLibraryExW(w!("gdi32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        let export = GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass"))?;
        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetSchedulingPriorityClass>(
            export,
        )
    };
    [(SCHEDULING_REALTIME, "realtime"), (SCHEDULING_HIGH, "high")]
        .into_iter()
        // SAFETY: changes the scheduling class of the current process only.
        .find(|(class, _)| unsafe { set_class(GetCurrentProcess(), *class) }.is_ok())
        .map(|(_, name)| name)
}
