//! The parts of NVIDIA Capture SDK 7 (`NvFBC.h`, `NvFBCToDx9Vid.h`, `NVFBC_DLL_VERSION` 0x70)
//! this program uses, laid out for 64-bit Windows.

use crate::gpu::ffi::raw_status;
use bytemuck::Zeroable;
use std::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

const DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "NvFBC's structures are a few kilobytes at most"
)]
pub(super) const fn struct_version(size: usize, version: u32) -> u32 {
    size as u32 | (version << 16) | (DLL_VERSION << 24)
}

pub(super) const STATUS_VER: u32 = struct_version(size_of::<StatusEx>(), 2);
pub(super) const CREATE_PARAMS_VER: u32 = struct_version(size_of::<CreateParams>(), 2);
pub(super) const TODX9VID_SETUP_PARAMS_VER: u32 = struct_version(size_of::<SetupParams>(), 3);
pub(super) const TODX9VID_GRAB_FRAME_PARAMS_VER: u32 =
    struct_version(size_of::<GrabFrameParams>(), 1);

// The values the header's macros evaluate to.
const _: () = assert!(STATUS_VER == 0x7002_0200);
const _: () = assert!(CREATE_PARAMS_VER == 0x7002_0200);
const _: () = assert!(TODX9VID_SETUP_PARAMS_VER == 0x7003_0100);
const _: () = assert!(TODX9VID_GRAB_FRAME_PARAMS_VER == 0x7001_0100);

/// `NVFBC_TO_DX9_VID`.
pub(super) const TO_DX9_VID: u32 = 0x2003;
/// `NVFBC_STATE_ENABLE`.
pub(super) const STATE_ENABLE: u32 = 1;
/// `NVFBC_TODX9VID_ARGB`: `D3DFMT_A8R8G8B8`.
pub(super) const TODX9VID_ARGB: u32 = 0;
/// `NVFBC_TODX9VID_NOWAIT`.
pub(super) const TODX9VID_NOWAIT: u32 = 0x1;

/// `NvFBCStatusEx::bIsCapturePossible`.
pub(super) const STATUS_CAPTURE_POSSIBLE: u32 = 1 << 0;
/// `NVFBC_TODX9VID_SETUP_PARAMS::bWithHWCursor`.
pub(super) const SETUP_WITH_HW_CURSOR: u32 = 1 << 0;

raw_status! {
    /// `NVFBCRESULT`.
    pub(in crate::gpu) struct Status, "unknown NvFBC status" {
        0 => "NVFBC_SUCCESS",
        -1 => "NVFBC_ERROR_GENERIC",
        -2 => "NVFBC_ERROR_INVALID_PARAM",
        -3 => "NVFBC_ERROR_INVALIDATED_SESSION",
        -4 => "NVFBC_ERROR_PROTECTED_CONTENT",
        -5 => "NVFBC_ERROR_DRIVER_FAILURE",
        -6 => "NVFBC_ERROR_CUDA_FAILURE",
        -7 => "NVFBC_ERROR_UNSUPPORTED",
        -8 => "NVFBC_ERROR_HW_ENC_FAILURE",
        -9 => "NVFBC_ERROR_INCOMPATIBLE_DRIVER",
        -10 => "NVFBC_ERROR_UNSUPPORTED_PLATFORM",
        -11 => "NVFBC_ERROR_OUT_OF_MEMORY",
        -12 => "NVFBC_ERROR_INVALID_PTR",
        -13 => "NVFBC_ERROR_INCOMPATIBLE_VERSION",
        -14 => "NVFBC_ERROR_OPT_CAPTURE_FAILURE",
        -15 => "NVFBC_ERROR_INSUFFICIENT_PRIVILEGES",
        -16 => "NVFBC_ERROR_INVALID_CALL",
        -17 => "NVFBC_ERROR_SYSTEM_ERROR",
        -18 => "NVFBC_ERROR_INVALID_TARGET",
        -19 => "NVFBC_ERROR_NVAPI_FAILURE",
        -20 => "NVFBC_ERROR_DYNAMIC_DISABLE",
        -21 => "NVFBC_ERROR_IPC_FAILURE",
        -22 => "NVFBC_ERROR_CURSOR_CAPTURE_FAILURE",
    }
}

impl Status {
    pub(super) const INVALIDATED_SESSION: Self = Self(-3);
    pub(super) const PROTECTED_CONTENT: Self = Self(-4);
    pub(super) const UNSUPPORTED: Self = Self(-7);
    pub(super) const INCOMPATIBLE_DRIVER: Self = Self(-9);
    pub(super) const UNSUPPORTED_PLATFORM: Self = Self(-10);
    pub(super) const INCOMPATIBLE_VERSION: Self = Self(-13);
    pub(super) const INSUFFICIENT_PRIVILEGES: Self = Self(-15);
    pub(super) const INVALID_TARGET: Self = Self(-18);
    pub(super) const DYNAMIC_DISABLE: Self = Self(-20);
}

/// `NvFBC_GetStatusEx`.
pub(super) type GetStatusEx = unsafe extern "system" fn(*mut StatusEx) -> Status;
/// `NvFBC_CreateEx`.
pub(super) type CreateEx = unsafe extern "system" fn(*mut CreateParams) -> Status;
/// `NvFBC_Enable`.
pub(super) type Enable = unsafe extern "system" fn(u32) -> Status;

/// `NvFBCStatusEx`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct StatusEx {
    pub(super) version: u32,
    pub(super) flags: u32,
    pub(super) nvfbc_version: u32,
    pub(super) adapter_index: u32,
    pub(super) private_data: *const c_void,
    pub(super) private_data_size: u32,
    _reserved: [u32; 59],
    _reserved_pointers: [*mut c_void; 31],
}
const _: () = assert!(size_of::<StatusEx>() == 512);
const _: () = assert!(offset_of!(StatusEx, adapter_index) == 12);
const _: () = assert!(offset_of!(StatusEx, private_data) == 16);
const _: () = assert!(offset_of!(StatusEx, private_data_size) == 24);

/// `NvFBCCreateParams`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct CreateParams {
    pub(super) version: u32,
    pub(super) interface_type: u32,
    pub(super) max_display_width: u32,
    pub(super) max_display_height: u32,
    pub(super) device: *mut c_void,
    pub(super) private_data: *const c_void,
    pub(super) private_data_size: u32,
    pub(super) interface_version: u32,
    /// The [`Interface`] asked for, or null when the call failed.
    pub(super) nvfbc: *mut c_void,
    pub(super) adapter_index: u32,
    pub(super) nvfbc_version: u32,
    _cuda_context: *mut c_void,
    _private_data2: *const c_void,
    _private_data2_size: u32,
    _reserved: [u32; 55],
    _reserved_pointers: [*mut c_void; 27],
}
const _: () = assert!(size_of::<CreateParams>() == 512);
const _: () = assert!(offset_of!(CreateParams, device) == 16);
const _: () = assert!(offset_of!(CreateParams, private_data) == 24);
const _: () = assert!(offset_of!(CreateParams, private_data_size) == 32);
const _: () = assert!(offset_of!(CreateParams, interface_version) == 36);
const _: () = assert!(offset_of!(CreateParams, nvfbc) == 40);
const _: () = assert!(offset_of!(CreateParams, adapter_index) == 48);
const _: () = assert!(offset_of!(CreateParams, nvfbc_version) == 52);
const _: () = assert!(offset_of!(CreateParams, _private_data2) == 64);
const _: () = assert!(offset_of!(CreateParams, _reserved) == 76);

/// `NVFBC_TODX9VID_OUT_BUF`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct OutputBuffer {
    pub(super) primary: *mut c_void,
    _secondary: *mut c_void,
}
const _: () = assert!(size_of::<OutputBuffer>() == 16);

/// `NVFBC_TODX9VID_SETUP_PARAMS_V3`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct SetupParams {
    pub(super) version: u32,
    pub(super) flags: u32,
    pub(super) mode: u32,
    pub(super) buffer_count: u32,
    _diff_map_block_size: u32,
    _stereo_format: u32,
    _diff_map_size: u32,
    _classification_map_size: u32,
    _classification_stamp_width: u32,
    _classification_stamp_height: u32,
    _diff_maps: *mut *mut c_void,
    _classification_maps: *mut *mut c_void,
    pub(super) buffers: *mut OutputBuffer,
    _cursor_capture_event: *mut c_void,
    _reserved: [u32; 22],
    _reserved_pointers: [*mut c_void; 12],
}
const _: () = assert!(size_of::<SetupParams>() == 256);
const _: () = assert!(offset_of!(SetupParams, mode) == 8);
const _: () = assert!(offset_of!(SetupParams, buffer_count) == 12);
const _: () = assert!(offset_of!(SetupParams, _diff_maps) == 40);
const _: () = assert!(offset_of!(SetupParams, buffers) == 56);
const _: () = assert!(offset_of!(SetupParams, _reserved) == 72);

/// `NVFBC_TODX9VID_GRAB_FRAME_PARAMS_V1`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct GrabFrameParams {
    pub(super) version: u32,
    pub(super) flags: u32,
    /// Target size and origin, for the scale and crop modes.
    _target_size: [u32; 2],
    _start: [u32; 2],
    /// Zero is `NVFBC_TODX9VID_SOURCEMODE_FULL`.
    _grab_mode: u32,
    pub(super) buffer_index: u32,
    pub(super) grab_info: *mut FrameGrabInfo,
    _wait_time: u32,
    _reserved: [u32; 23],
    _reserved_pointers: [*mut c_void; 15],
}
const _: () = assert!(size_of::<GrabFrameParams>() == 256);
const _: () = assert!(offset_of!(GrabFrameParams, _grab_mode) == 24);
const _: () = assert!(offset_of!(GrabFrameParams, buffer_index) == 28);
const _: () = assert!(offset_of!(GrabFrameParams, grab_info) == 32);
const _: () = assert!(offset_of!(GrabFrameParams, _reserved) == 44);

/// `NvFBCFrameGrabInfo`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct FrameGrabInfo {
    pub(super) width: u32,
    pub(super) height: u32,
    /// Row pitch in pixels.
    pub(super) buffer_width: u32,
    _reserved: u32,
    _overlay_active: i32,
    pub(super) must_recreate: i32,
    _first_buffer_to_wait_mode_used: [u32; 10],
    _reserved2: [u32; 11],
}
const _: () = assert!(size_of::<FrameGrabInfo>() == 108);
const _: () = assert!(offset_of!(FrameGrabInfo, must_recreate) == 20);
const _: () = assert!(offset_of!(FrameGrabInfo, _reserved2) == 64);

/// An interface object from `NvFBC_CreateEx`. It is valid until `release`, while the library is
/// loaded. The method table `V` lives in the library.
#[repr(C)]
pub(super) struct Interface<V> {
    pub(super) vtable: *const V,
}

/// A method table in declaration order. The interface is pure virtual and has no virtual
/// destructor.
pub(super) trait Vtable: Sized {
    fn release(&self) -> Release<Self>;
}

pub(super) type Release<V> = unsafe extern "system" fn(*mut Interface<V>) -> Status;

/// `INvFBCToDx9Vid_v3`.
pub(super) type ToDx9Vid = Interface<ToDx9VidVtable>;

#[repr(C)]
pub(super) struct ToDx9VidVtable {
    pub(super) set_up: unsafe extern "system" fn(*mut ToDx9Vid, *mut SetupParams) -> Status,
    pub(super) grab_frame: unsafe extern "system" fn(*mut ToDx9Vid, *mut GrabFrameParams) -> Status,
    _gpu_based_cpu_sleep: unsafe extern "system" fn(*mut ToDx9Vid, i64) -> Status,
    release: Release<Self>,
    _cursor_capture: unsafe extern "system" fn(*mut ToDx9Vid, *mut c_void) -> Status,
}
const _: () = assert!(size_of::<ToDx9VidVtable>() == 5 * size_of::<usize>());
const _: () = assert!(offset_of!(ToDx9VidVtable, release) == 3 * size_of::<usize>());
const _: () = assert!(offset_of!(ToDx9VidVtable, _cursor_capture) == 4 * size_of::<usize>());

impl Vtable for ToDx9VidVtable {
    fn release(&self) -> Release<Self> {
        self.release
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_show_their_name_with_the_code() {
        assert_eq!(
            Status::INVALIDATED_SESSION.to_string(),
            "NVFBC_ERROR_INVALIDATED_SESSION (-3)"
        );
        assert_eq!(Status(-99).to_string(), "unknown NvFBC status (-99)");
        assert!(Status::SUCCESS.ok().is_ok());
        assert_eq!(Status::UNSUPPORTED.ok(), Err(Status::UNSUPPORTED));
    }
}
