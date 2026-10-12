//! The parts of NVIDIA's Capture SDK CUDA interface for Windows (`NvFBCCuda.h`, SDK 7) this
//! program uses, laid out like the Direct3D 9 interface in the parent module's bindings.
//!
//! The header is not on this machine; the layouts come from public copies of the SDK 7 header
//! (`docs/nvfbc-cuda.md` lists them) and are checked at compile time against what it produces
//! for 64-bit Windows. The capture object is a C++ class with only pure virtual methods and no
//! virtual destructor; its table is spelled out in header order, which differs from the
//! Direct3D 9 interface's: the size query comes first and the release last.

use super::super::ffi::{FrameGrabInfo, Status, Zeroed, struct_version};
use std::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

pub(super) const SETUP_PARAMS_VER: u32 = struct_version(size_of::<SetupParams>(), 1);
pub(super) const GRAB_FRAME_PARAMS_VER: u32 = struct_version(size_of::<GrabFrameParams>(), 1);

// What the header's macros evaluate to for 64-bit Windows.
const _: () = assert!(SETUP_PARAMS_VER == 0x7001_0200);
const _: () = assert!(GRAB_FRAME_PARAMS_VER == 0x7001_0200);

/// `NVFBC_SHARED_CUDA`: capture into CUDA device memory. The SDK 7 header gives 0x1007; the
/// SDK 5 header gave 0x1006, so the value moves with the interface version, like the
/// `INvFBCCuda_v3` table below.
pub(super) const SHARED_CUDA: u32 = 0x1007;
/// `NVFBC_TOCUDA_NOWAIT`: return the current picture rather than wait for a new one.
pub(super) const TOCUDA_NOWAIT: u32 = 0x1;
/// `NVFBC_TOCUDA_WITH_HWCURSOR`: draw the mouse cursor into the picture.
pub(super) const TOCUDA_WITH_HW_CURSOR: u32 = 0x4;
/// `NVFBC_TOCUDA_ARGB`: 32-bit words with blue in the low byte, as `D3DFMT_A8R8G8B8`.
pub(super) const TOCUDA_ARGB: u32 = 0;

/// `NVFBC_CUDA_SETUP_PARAMS_V1`.
#[repr(C)]
pub(super) struct SetupParams {
    pub(super) version: u32,
    /// Bit fields: `bEnableSeparateCursorCapture`, then `bHDRRequest`; both left 0.
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
    /// `pCUDADeviceBuffer`: a `CUdeviceptr`, which the header types as a pointer.
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

/// An `INvFBCCuda_v3` object: a pointer to its method table.
///
/// Only ever seen behind a pointer `NvFBC_CreateEx` returned. Such a pointer, and the table it
/// leads to, stay valid until `release` is called on it, provided the library that created it
/// stays loaded; the table itself lives in the library.
#[repr(C)]
pub(super) struct ToCuda {
    pub(super) vtable: *const ToCudaVtable,
}

/// `INvFBCCuda_v3`'s methods, in declaration order.
#[repr(C)]
pub(super) struct ToCudaVtable {
    pub(super) get_max_buffer_size: unsafe extern "system" fn(*mut ToCuda, *mut u32) -> Status,
    pub(super) set_up: unsafe extern "system" fn(*mut ToCuda, *mut SetupParams) -> Status,
    pub(super) grab_frame: unsafe extern "system" fn(*mut ToCuda, *mut GrabFrameParams) -> Status,
    _gpu_based_cpu_sleep: unsafe extern "system" fn(*mut ToCuda, i64) -> Status,
    _cursor_capture: unsafe extern "system" fn(*mut ToCuda, *mut c_void) -> Status,
    pub(super) release: unsafe extern "system" fn(*mut ToCuda) -> Status,
}
const _: () = assert!(size_of::<ToCudaVtable>() == 6 * size_of::<usize>());
const _: () = assert!(offset_of!(ToCudaVtable, release) == 5 * size_of::<usize>());
