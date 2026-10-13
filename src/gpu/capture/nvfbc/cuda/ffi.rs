//! The parts of NVIDIA Capture SDK 7's `NvFBCCuda.h` this program uses, laid out for 64-bit
//! Windows from public copies of the header.

use crate::gpu::capture::nvfbc::ffi::{
    FrameGrabInfo, Interface, Release, Status, Vtable, struct_version,
};
use crate::gpu::ffi::Zeroed;
use std::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

pub(super) const SETUP_PARAMS_VER: u32 = struct_version(size_of::<SetupParams>(), 1);
pub(super) const GRAB_FRAME_PARAMS_VER: u32 = struct_version(size_of::<GrabFrameParams>(), 1);

// What the header's macros evaluate to.
const _: () = assert!(SETUP_PARAMS_VER == 0x7001_0200);
const _: () = assert!(GRAB_FRAME_PARAMS_VER == 0x7001_0200);

/// `NVFBC_SHARED_CUDA`: 0x1007 in SDK 7, 0x1006 in SDK 5; it moves with the interface version.
pub(super) const SHARED_CUDA: u32 = 0x1007;
/// `NVFBC_TOCUDA_NOWAIT`.
pub(super) const TOCUDA_NOWAIT: u32 = 0x1;
/// `NVFBC_TOCUDA_WITH_HWCURSOR`.
pub(super) const TOCUDA_WITH_HW_CURSOR: u32 = 0x4;
/// `NVFBC_TOCUDA_ARGB`: as `D3DFMT_A8R8G8B8`.
pub(super) const TOCUDA_ARGB: u32 = 0;

/// `NVFBC_CUDA_SETUP_PARAMS_V1`.
#[repr(C)]
pub(super) struct SetupParams {
    pub(super) version: u32,
    /// `bEnableSeparateCursorCapture`, then `bHDRRequest`.
    pub(super) flags: u32,
    _cursor_capture_event: *mut c_void,
    pub(super) format: u32,
    _reserved: [u32; 61],
    _reserved_pointers: [*mut c_void; 31],
}
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for SetupParams {}
const _: () = assert!(size_of::<SetupParams>() == 512);
const _: () = assert!(offset_of!(SetupParams, _cursor_capture_event) == 8);
const _: () = assert!(offset_of!(SetupParams, format) == 16);
const _: () = assert!(offset_of!(SetupParams, _reserved) == 20);
const _: () = assert!(offset_of!(SetupParams, _reserved_pointers) == 264);

/// `NVFBC_CUDA_GRAB_FRAME_PARAMS_V1`.
#[repr(C)]
pub(super) struct GrabFrameParams {
    pub(super) version: u32,
    pub(super) flags: u32,
    /// `pCUDADeviceBuffer`: a `CUdeviceptr` typed as a pointer.
    pub(super) cuda_device_buffer: *mut c_void,
    pub(super) grab_info: *mut FrameGrabInfo,
    _wait_time: u32,
    _reserved: [u32; 61],
    _reserved_pointers: [*mut c_void; 30],
}
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for GrabFrameParams {}
const _: () = assert!(size_of::<GrabFrameParams>() == 512);
const _: () = assert!(offset_of!(GrabFrameParams, cuda_device_buffer) == 8);
const _: () = assert!(offset_of!(GrabFrameParams, grab_info) == 16);
const _: () = assert!(offset_of!(GrabFrameParams, _wait_time) == 24);
const _: () = assert!(offset_of!(GrabFrameParams, _reserved) == 28);
const _: () = assert!(offset_of!(GrabFrameParams, _reserved_pointers) == 272);

/// `INvFBCCuda_v3`.
pub(super) type ToCuda = Interface<ToCudaVtable>;

/// The size query first, the release last.
#[repr(C)]
pub(super) struct ToCudaVtable {
    pub(super) get_max_buffer_size: unsafe extern "system" fn(*mut ToCuda, *mut u32) -> Status,
    pub(super) set_up: unsafe extern "system" fn(*mut ToCuda, *mut SetupParams) -> Status,
    pub(super) grab_frame: unsafe extern "system" fn(*mut ToCuda, *mut GrabFrameParams) -> Status,
    _gpu_based_cpu_sleep: unsafe extern "system" fn(*mut ToCuda, i64) -> Status,
    _cursor_capture: unsafe extern "system" fn(*mut ToCuda, *mut c_void) -> Status,
    release: Release<Self>,
}
const _: () = assert!(size_of::<ToCudaVtable>() == 6 * size_of::<usize>());
const _: () = assert!(offset_of!(ToCudaVtable, release) == 5 * size_of::<usize>());

impl Vtable for ToCudaVtable {
    fn release(&self) -> Release<Self> {
        self.release
    }
}
