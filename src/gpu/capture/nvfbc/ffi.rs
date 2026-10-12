//! The parts of NVIDIA Capture SDK 7 (`NvFBC.h`, `NvFBCToDx9Vid.h`, `NVFBC_DLL_VERSION` 0x70)
//! this program uses, laid out for 64-bit Windows; [`ProcessSource`] is not in the SDK.

use std::{
    ffi::c_void,
    fmt,
    mem::{offset_of, size_of},
};

const DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION`.
#[expect(clippy::cast_possible_truncation)]
pub(super) const fn struct_version(size: usize, version: u32) -> u32 {
    size as u32 | (version << 16) | (DLL_VERSION << 24)
}

pub(super) const STATUS_VER: u32 = struct_version(size_of::<StatusEx>(), 2);
pub(super) const CREATE_PARAMS_VER: u32 = struct_version(size_of::<CreateParams>(), 2);
pub(super) const TODX9VID_SETUP_PARAMS_VER: u32 = struct_version(size_of::<SetupParams>(), 3);
pub(super) const TODX9VID_GRAB_FRAME_PARAMS_VER: u32 =
    struct_version(size_of::<GrabFrameParams>(), 1);
pub(super) const PROCESS_SOURCE_VER: u32 = struct_version(size_of::<ProcessSource>(), 1);

// What the header's macros evaluate to.
const _: () = assert!(STATUS_VER == 0x7002_0200);
const _: () = assert!(CREATE_PARAMS_VER == 0x7002_0200);
const _: () = assert!(TODX9VID_SETUP_PARAMS_VER == 0x7003_0100);
const _: () = assert!(TODX9VID_GRAB_FRAME_PARAMS_VER == 0x7001_0100);
const _: () = assert!(PROCESS_SOURCE_VER == 0x7001_00C8);

/// `NVFBC_TO_DX9_VID`.
pub(super) const TO_DX9_VID: u32 = 0x2003;
/// `NVFBC_STATE_ENABLE`.
pub(super) const STATE_ENABLE: u32 = 1;
/// `NVFBC_TODX9VID_ARGB`: `D3DFMT_A8R8G8B8`.
pub(super) const TODX9VID_ARGB: u32 = 0;
/// `NVFBC_TODX9VID_NV12`: BT.601 limited range. Not in the SDK 7.1 guide; the driver accepts it.
pub(super) const TODX9VID_NV12: u32 = 1;
/// `NVFBC_TODX9VID_SOURCEMODE_*`.
pub(super) const TODX9VID_SOURCEMODE_FULL: u32 = 0;
pub(super) const TODX9VID_SOURCEMODE_SCALE: u32 = 1;
pub(super) const TODX9VID_SOURCEMODE_CROP: u32 = 2;
/// `NVFBC_TODX9VID_NOWAIT`.
pub(super) const TODX9VID_NOWAIT: u32 = 0x1;
/// `NVFBC_TODX9VID_WAIT_WITH_TIMEOUT`.
pub(super) const TODX9VID_WAIT_WITH_TIMEOUT: u32 = 0x10;
/// `ProcessSource::capture_mode` for one process's presented frames.
pub(super) const CAPTURE_MODE_PROCESS: u32 = 1;

/// `NvFBCStatusEx::bIsCapturePossible`.
pub(super) const STATUS_CAPTURE_POSSIBLE: u32 = 1 << 0;
/// `NVFBC_TODX9VID_SETUP_PARAMS::bWithHWCursor`.
pub(super) const SETUP_WITH_HW_CURSOR: u32 = 1 << 0;

/// `NVFBCRESULT`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gpu) struct Status(i32);

impl Status {
    pub(super) const SUCCESS: Self = Self(0);
    pub(super) const INVALIDATED_SESSION: Self = Self(-3);
    pub(super) const PROTECTED_CONTENT: Self = Self(-4);
    pub(super) const UNSUPPORTED: Self = Self(-7);
    pub(super) const INCOMPATIBLE_DRIVER: Self = Self(-9);
    pub(super) const UNSUPPORTED_PLATFORM: Self = Self(-10);
    pub(super) const INCOMPATIBLE_VERSION: Self = Self(-13);
    pub(super) const INSUFFICIENT_PRIVILEGES: Self = Self(-15);
    pub(super) const INVALID_TARGET: Self = Self(-18);
    pub(super) const DYNAMIC_DISABLE: Self = Self(-20);

    pub(super) fn ok(self) -> Result<(), Self> {
        if self == Self::SUCCESS {
            Ok(())
        } else {
            Err(self)
        }
    }

    const fn name(self) -> &'static str {
        match self.0 {
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
            _ => "unknown NvFBC status",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name(), self.0)
    }
}

/// # Safety
/// Implementors contain only integers and raw pointers, so all-zero is a valid value.
pub(super) unsafe trait Zeroed: Sized {
    fn zeroed() -> Self {
        // SAFETY: guaranteed by the trait's contract.
        unsafe { std::mem::zeroed() }
    }
}

/// `NvFBC_GetStatusEx`.
pub(super) type GetStatusEx = unsafe extern "system" fn(*mut StatusEx) -> Status;
/// `NvFBC_CreateEx`.
pub(super) type CreateEx = unsafe extern "system" fn(*mut CreateParams) -> Status;
/// `NvFBC_Enable`.
pub(super) type Enable = unsafe extern "system" fn(u32) -> Status;

/// `NvFBCStatusEx`.
#[repr(C)]
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
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for StatusEx {}
const _: () = assert!(size_of::<StatusEx>() == 512);
const _: () = assert!(offset_of!(StatusEx, adapter_index) == 12);
const _: () = assert!(offset_of!(StatusEx, private_data) == 16);
const _: () = assert!(offset_of!(StatusEx, private_data_size) == 24);

/// `NvFBCCreateParams`.
#[repr(C)]
pub(super) struct CreateParams {
    pub(super) version: u32,
    pub(super) interface_type: u32,
    pub(super) max_display_width: u32,
    pub(super) max_display_height: u32,
    pub(super) device: *mut c_void,
    pub(super) private_data: *const c_void,
    pub(super) private_data_size: u32,
    pub(super) interface_version: u32,
    pub(super) nvfbc: *mut ToDx9Vid,
    pub(super) adapter_index: u32,
    pub(super) nvfbc_version: u32,
    _cuda_context: *mut c_void,
    /// `pPrivateData2`, "private data, optional" in the header; a [`ProcessSource`] here.
    pub(super) private_data2: *const c_void,
    pub(super) private_data2_size: u32,
    _reserved: [u32; 55],
    _reserved_pointers: [*mut c_void; 27],
}
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for CreateParams {}
const _: () = assert!(size_of::<CreateParams>() == 512);
const _: () = assert!(offset_of!(CreateParams, device) == 16);
const _: () = assert!(offset_of!(CreateParams, private_data) == 24);
const _: () = assert!(offset_of!(CreateParams, private_data_size) == 32);
const _: () = assert!(offset_of!(CreateParams, interface_version) == 36);
const _: () = assert!(offset_of!(CreateParams, nvfbc) == 40);
const _: () = assert!(offset_of!(CreateParams, adapter_index) == 48);
const _: () = assert!(offset_of!(CreateParams, nvfbc_version) == 52);
const _: () = assert!(offset_of!(CreateParams, private_data2) == 64);
const _: () = assert!(offset_of!(CreateParams, private_data2_size) == 72);
const _: () = assert!(offset_of!(CreateParams, _reserved) == 76);

/// `NvFBCCreateParamsPrivateData`, not in the public SDK: name, 200-byte size and leading
/// fields as rbuf read them out of `NvFBC64.dll`.
#[repr(C)]
pub(super) struct ProcessSource {
    pub(super) version: u32,
    pub(super) capture_mode: u32,
    pub(super) target_pid: u32,
    _reserved: [u32; 47],
}
// SAFETY: integers only.
unsafe impl Zeroed for ProcessSource {}
const _: () = assert!(size_of::<ProcessSource>() == 200);
const _: () = assert!(offset_of!(ProcessSource, target_pid) == 8);

/// `NVFBC_TODX9VID_OUT_BUF`.
#[repr(C)]
pub(super) struct OutputBuffer {
    pub(super) primary: *mut c_void,
    _secondary: *mut c_void,
}
// SAFETY: raw pointers only.
unsafe impl Zeroed for OutputBuffer {}
const _: () = assert!(size_of::<OutputBuffer>() == 16);

/// `NVFBC_TODX9VID_SETUP_PARAMS_V3`.
#[repr(C)]
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
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for SetupParams {}
const _: () = assert!(size_of::<SetupParams>() == 256);
const _: () = assert!(offset_of!(SetupParams, mode) == 8);
const _: () = assert!(offset_of!(SetupParams, buffer_count) == 12);
const _: () = assert!(offset_of!(SetupParams, _diff_maps) == 40);
const _: () = assert!(offset_of!(SetupParams, buffers) == 56);
const _: () = assert!(offset_of!(SetupParams, _reserved) == 72);

/// `NVFBC_TODX9VID_GRAB_FRAME_PARAMS_V1`.
#[repr(C)]
pub(super) struct GrabFrameParams {
    pub(super) version: u32,
    pub(super) flags: u32,
    /// For `TODX9VID_SOURCEMODE_SCALE` and `_CROP`.
    pub(super) target_width: u32,
    pub(super) target_height: u32,
    /// For `TODX9VID_SOURCEMODE_CROP`.
    pub(super) start_x: u32,
    pub(super) start_y: u32,
    pub(super) grab_mode: u32,
    pub(super) buffer_index: u32,
    pub(super) grab_info: *mut FrameGrabInfo,
    /// Milliseconds, with `TODX9VID_WAIT_WITH_TIMEOUT`.
    pub(super) wait_time: u32,
    _reserved: [u32; 23],
    _reserved_pointers: [*mut c_void; 15],
}
// SAFETY: integers and raw pointers only.
unsafe impl Zeroed for GrabFrameParams {}
const _: () = assert!(size_of::<GrabFrameParams>() == 256);
const _: () = assert!(offset_of!(GrabFrameParams, target_width) == 8);
const _: () = assert!(offset_of!(GrabFrameParams, start_x) == 16);
const _: () = assert!(offset_of!(GrabFrameParams, grab_mode) == 24);
const _: () = assert!(offset_of!(GrabFrameParams, buffer_index) == 28);
const _: () = assert!(offset_of!(GrabFrameParams, grab_info) == 32);
const _: () = assert!(offset_of!(GrabFrameParams, wait_time) == 40);
const _: () = assert!(offset_of!(GrabFrameParams, _reserved) == 44);

/// `NvFBCFrameGrabInfo`.
#[repr(C)]
pub(super) struct FrameGrabInfo {
    pub(super) width: u32,
    pub(super) height: u32,
    /// Row pitch in pixels.
    pub(super) buffer_width: u32,
    _reserved: u32,
    _overlay_active: i32,
    pub(super) must_recreate: i32,
    _first_buffer_to_igpu_capture: [u32; 6],
    /// `dwSourcePID`.
    pub(super) source_pid: u32,
    _reserved3: u32,
    _hdr_bits: u32,
    /// `dwWaitModeUsed`; can differ from the flags asked for.
    pub(super) wait_mode_used: u32,
    _reserved2: [u32; 11],
}
// SAFETY: integers only.
unsafe impl Zeroed for FrameGrabInfo {}
const _: () = assert!(size_of::<FrameGrabInfo>() == 108);
const _: () = assert!(offset_of!(FrameGrabInfo, must_recreate) == 20);
const _: () = assert!(offset_of!(FrameGrabInfo, source_pid) == 48);
const _: () = assert!(offset_of!(FrameGrabInfo, wait_mode_used) == 60);

/// `INvFBCToDx9Vid_v3`: valid from `NvFBC_CreateEx` until `release`, while the library is
/// loaded; the method table lives in the library.
#[repr(C)]
pub(super) struct ToDx9Vid {
    pub(super) vtable: *const ToDx9VidVtable,
}

/// In declaration order; pure virtual, no virtual destructor.
#[repr(C)]
pub(super) struct ToDx9VidVtable {
    pub(super) set_up: unsafe extern "system" fn(*mut ToDx9Vid, *mut SetupParams) -> Status,
    pub(super) grab_frame: unsafe extern "system" fn(*mut ToDx9Vid, *mut GrabFrameParams) -> Status,
    _gpu_based_cpu_sleep: unsafe extern "system" fn(*mut ToDx9Vid, i64) -> Status,
    pub(super) release: unsafe extern "system" fn(*mut ToDx9Vid) -> Status,
    _cursor_capture: unsafe extern "system" fn(*mut ToDx9Vid, *mut c_void) -> Status,
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
