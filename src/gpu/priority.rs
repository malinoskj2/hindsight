//! Thread, process and GPU scheduling priority for the capture thread.

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

/// `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH` (`d3dkmthk.h`); needs no elevation.
const SCHEDULING_HIGH: i32 = 4;

/// `D3DKMTSetProcessSchedulingPriorityClass`; `gdi32.dll` exports it without an import library.
type SetSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> NTSTATUS;

pub(super) struct MultimediaThread(HANDLE);

impl Drop for MultimediaThread {
    fn drop(&mut self) {
        // SAFETY: reverts the registration this handle stands for.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

pub(super) fn join_multimedia_class() -> windows::core::Result<MultimediaThread> {
    let mut index = 0;
    // SAFETY: built-in task name; `index` is written on success.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &raw mut index) }?;
    let joined = MultimediaThread(handle);
    // SAFETY: the handle was just issued for this thread and is still registered.
    unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) }?;
    Ok(joined)
}

pub(super) fn raise_thread() -> windows::core::Result<()> {
    // SAFETY: changes the priority of the calling thread only.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
}

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

/// High, not realtime: realtime would starve the game and the compositor of the GPU.
pub(super) fn raise_gpu_class() -> bool {
    let Some(set_class) = scheduling_class_setter() else {
        return false;
    };
    // SAFETY: changes the scheduling class of the current process only.
    unsafe { set_class(GetCurrentProcess(), SCHEDULING_HIGH) }.is_ok()
}

fn scheduling_class_setter() -> Option<SetSchedulingPriorityClass> {
    // Never freed; `gdi32.dll` is loaded in every GUI process anyway.
    // SAFETY: loads a system library by name.
    let gdi =
        unsafe { LoadLibraryExW(w!("gdi32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
    // SAFETY: looks a symbol up in the loaded library.
    let export = unsafe { GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) }?;
    // SAFETY: the export has this signature since Windows Vista.
    Some(unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetSchedulingPriorityClass>(
            export,
        )
    })
}
