//! Thread, process and GPU scheduling priority for the capture thread.

use crate::gpu::{ffi::Library, win32::cb_size};
use windows::{
    Win32::{
        Foundation::{HANDLE, NTSTATUS},
        System::Threading::{
            AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
            AvSetMmThreadPriority, GetCurrentProcess, GetCurrentThread,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation, SetThreadPriority,
            THREAD_PRIORITY_HIGHEST,
        },
    },
    core::{s, w},
};

/// `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH` (`d3dkmthk.h`); needs no elevation.
const SCHEDULING_HIGH: i32 = 4;

/// `D3DKMTSetProcessSchedulingPriorityClass`; `gdi32.dll` exports it without an import library.
type SetSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> NTSTATUS;

pub(super) struct MultimediaThread(HANDLE);

#[expect(unsafe_code, reason = "calls AvRevertMmThreadCharacteristics")]
impl Drop for MultimediaThread {
    fn drop(&mut self) {
        // SAFETY: reverts the registration this handle stands for.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

#[expect(unsafe_code, reason = "calls the MMCSS API")]
pub(super) fn join_multimedia_class() -> windows::core::Result<MultimediaThread> {
    let mut index = 0;
    // SAFETY: built-in task name; `index` is written on success.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &raw mut index) }?;
    let joined = MultimediaThread(handle);
    // SAFETY: the handle was just issued for this thread and is still registered.
    unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) }?;
    Ok(joined)
}

#[expect(unsafe_code, reason = "calls SetThreadPriority")]
pub(super) fn raise_thread() -> windows::core::Result<()> {
    // SAFETY: returns a pseudo-handle that needs no closing.
    let thread = unsafe { GetCurrentThread() };
    // SAFETY: changes the priority of the calling thread only.
    unsafe { SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST) }
}

#[expect(unsafe_code, reason = "calls SetProcessInformation")]
pub(super) fn opt_out_of_throttling() -> windows::core::Result<()> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    let size = cb_size::<PROCESS_POWER_THROTTLING_STATE>();
    // SAFETY: returns a pseudo-handle that needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `state` is a `PROCESS_POWER_THROTTLING_STATE` of `size` bytes, live for the call.
    unsafe {
        SetProcessInformation(
            process,
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size,
        )
    }
}

#[expect(unsafe_code, reason = "calls D3DKMTSetProcessSchedulingPriorityClass")]
/// High, not realtime: realtime would starve the game and the compositor of the GPU.
pub(super) fn raise_gpu_class() -> windows::core::Result<()> {
    let gdi = Library::load(w!("gdi32.dll"))?;
    // SAFETY: the export has this signature since Windows Vista.
    let set_class = unsafe {
        gdi.export::<SetSchedulingPriorityClass>(s!("D3DKMTSetProcessSchedulingPriorityClass"))
    }
    .ok_or_else(windows::core::Error::from_thread)?;
    // SAFETY: returns a pseudo-handle that needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `gdi` stays loaded across the call, which changes the scheduling class of the
    // current process only.
    unsafe { set_class(process, SCHEDULING_HIGH) }.ok()
}
