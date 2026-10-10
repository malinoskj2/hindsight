//! The parts of NVIDIA's Video Codec SDK interface (`nvEncodeAPI.h`, API 12.1) this program uses.
//!
//! Only the fields that are set or read have names; the rest of each structure is kept as
//! opaque padding of the same size. Every size and named offset is checked at compile time
//! against the values the C header produces for 64-bit Windows, so a slip in a layout fails the
//! build instead of corrupting memory in the driver. API 12.1 is accepted by drivers 531 and
//! newer, which covers every GPU with an AV1 encoder.

use std::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
};
use windows::core::GUID;

pub(super) const API_MAJOR: u32 = 12;
pub(super) const API_MINOR: u32 = 1;
pub(super) const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);

const fn struct_version(version: u32) -> u32 {
    API_VERSION | (version << 16) | (0x7 << 28)
}

pub(super) const FUNCTION_LIST_VER: u32 = struct_version(2);
pub(super) const OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub(super) const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub(super) const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
pub(super) const INITIALIZE_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub(super) const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub(super) const LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);
pub(super) const MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub(super) const REGISTER_RESOURCE_VER: u32 = struct_version(4);
pub(super) const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub(super) const EVENT_PARAMS_VER: u32 = struct_version(1);
pub(super) const CAPS_PARAM_VER: u32 = struct_version(1);

// The header's own values for the versions above.
const _: () = assert!(API_VERSION == 0x0100_000C);
const _: () = assert!(FUNCTION_LIST_VER == 0x7102_000C);
const _: () = assert!(CONFIG_VER == 0xF108_000C);
const _: () = assert!(PIC_PARAMS_VER == 0xF106_000C);

pub(super) const CODEC_H264: GUID = GUID::from_values(
    0x6bc8_2762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub(super) const CODEC_HEVC: GUID = GUID::from_values(
    0x790c_dc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub(super) const CODEC_AV1: GUID = GUID::from_values(
    0x0a35_2289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
pub(super) const PROFILE_H264_HIGH: GUID = GUID::from_values(
    0xe7cb_c309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
pub(super) const PROFILE_HEVC_MAIN: GUID = GUID::from_values(
    0xb514_c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
pub(super) const PROFILE_AV1_MAIN: GUID = GUID::from_values(
    0x5f2a_39f5,
    0xf14e,
    0x4f95,
    [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
);
/// The middle of the seven speed/quality presets.
pub(super) const PRESET_P4: GUID = GUID::from_values(
    0x90a7_b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);

pub(super) const CAPS_WIDTH_MAX: u32 = 16;
pub(super) const CAPS_HEIGHT_MAX: u32 = 17;
pub(super) const TUNING_HIGH_QUALITY: u32 = 1;
pub(super) const RATE_CONTROL_VBR: u32 = 1;
pub(super) const MULTI_PASS_DISABLED: u32 = 0;
pub(super) const DEVICE_TYPE_DIRECTX: u32 = 0;
pub(super) const INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
pub(super) const INPUT_IMAGE: u32 = 0;
/// 32-bit words with blue in the low byte: `DXGI_FORMAT_B8G8R8A8_UNORM`.
pub(super) const BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
pub(super) const PIC_STRUCT_FRAME: u32 = 1;
pub(super) const PIC_TYPE_IDR: u32 = 3;
pub(super) const PIC_FLAG_EOS: u32 = 0x8;

/// `NV_ENC_RC_PARAMS` flag: rate control looks ahead (and holds frames back).
pub(super) const RC_ENABLE_LOOKAHEAD: u32 = 1 << 5;
/// `NV_ENC_CONFIG_H264` flag: SPS and PPS before every IDR frame.
pub(super) const H264_REPEAT_SPS_PPS: u32 = 1 << 12;
/// `NV_ENC_CONFIG_HEVC` flags and fields.
pub(super) const HEVC_REPEAT_SPS_PPS: u32 = 1 << 7;
pub(super) const HEVC_CHROMA_FORMAT_SHIFT: u32 = 9;
pub(super) const HEVC_CHROMA_FORMAT_MASK: u32 = 0b11 << 9;
pub(super) const HEVC_BIT_DEPTH_MASK: u32 = 0b111 << 11;
/// `NV_ENC_CONFIG_AV1` flags and fields.
pub(super) const AV1_ANNEX_B: u32 = 1 << 0;
pub(super) const AV1_TIMING_INFO: u32 = 1 << 1;
pub(super) const AV1_DECODER_MODEL_INFO: u32 = 1 << 2;
pub(super) const AV1_FRAME_ID_NUMBERS: u32 = 1 << 3;
pub(super) const AV1_DISABLE_SEQUENCE_HEADER: u32 = 1 << 4;
pub(super) const AV1_REPEAT_SEQUENCE_HEADER: u32 = 1 << 5;
pub(super) const AV1_CHROMA_FORMAT_SHIFT: u32 = 7;
pub(super) const AV1_CHROMA_FORMAT_MASK: u32 = 0b11 << 7;
pub(super) const AV1_BIT_DEPTH_MASK: u32 = 0b11_1111 << 12;

pub(super) const CHROMA_FORMAT_420: u32 = 1;
pub(super) const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
pub(super) const BT709: u32 = 1;

/// `NVENCSTATUS`; kept as an integer, since a driver may return values this program does not
/// know.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Status(pub(super) i32);

impl Status {
    pub(super) const SUCCESS: Self = Self(0);
    pub(super) const INVALID_VERSION: Self = Self(15);

    pub(super) const fn name(self) -> &'static str {
        match self.0 {
            0 => "NV_ENC_SUCCESS",
            1 => "NV_ENC_ERR_NO_ENCODE_DEVICE",
            2 => "NV_ENC_ERR_UNSUPPORTED_DEVICE",
            3 => "NV_ENC_ERR_INVALID_ENCODERDEVICE",
            4 => "NV_ENC_ERR_INVALID_DEVICE",
            5 => "NV_ENC_ERR_DEVICE_NOT_EXIST",
            6 => "NV_ENC_ERR_INVALID_PTR",
            7 => "NV_ENC_ERR_INVALID_EVENT",
            8 => "NV_ENC_ERR_INVALID_PARAM",
            9 => "NV_ENC_ERR_INVALID_CALL",
            10 => "NV_ENC_ERR_OUT_OF_MEMORY",
            11 => "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
            12 => "NV_ENC_ERR_UNSUPPORTED_PARAM",
            13 => "NV_ENC_ERR_LOCK_BUSY",
            14 => "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
            15 => "NV_ENC_ERR_INVALID_VERSION",
            16 => "NV_ENC_ERR_MAP_FAILED",
            17 => "NV_ENC_ERR_NEED_MORE_INPUT",
            18 => "NV_ENC_ERR_ENCODER_BUSY",
            19 => "NV_ENC_ERR_EVENT_NOT_REGISTERD",
            20 => "NV_ENC_ERR_GENERIC",
            21 => "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
            22 => "NV_ENC_ERR_UNIMPLEMENTED",
            23 => "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
            24 => "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
            25 => "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
            26 => "NV_ENC_ERR_NEED_MORE_OUTPUT",
            _ => "unknown NVENC status",
        }
    }
}

/// Structures for which every bit being zero is a valid value, as the API expects of the parts
/// a caller does not set.
///
/// # Safety
///
/// Implementors contain only integers, GUIDs, raw pointers, optional function pointers and
/// unions or arrays of those.
pub(super) unsafe trait Zeroed: Sized {
    fn zeroed() -> Self {
        // SAFETY: guaranteed by the trait's contract.
        unsafe { std::mem::zeroed() }
    }
}

pub(super) type Session = *mut c_void;
type Entry = *const c_void;

pub(super) type GetMaxSupportedVersion = unsafe extern "system" fn(*mut u32) -> Status;
pub(super) type CreateInstance = unsafe extern "system" fn(*mut FunctionList) -> Status;
pub(super) type OpenEncodeSessionEx =
    unsafe extern "system" fn(*mut OpenEncodeSessionExParams, *mut Session) -> Status;
pub(super) type GetEncodeGuidCount = unsafe extern "system" fn(Session, *mut u32) -> Status;
pub(super) type GetEncodeGuids =
    unsafe extern "system" fn(Session, *mut GUID, u32, *mut u32) -> Status;
pub(super) type GetEncodeCaps =
    unsafe extern "system" fn(Session, GUID, *mut CapsParam, *mut i32) -> Status;
pub(super) type GetEncodePresetConfigEx =
    unsafe extern "system" fn(Session, GUID, GUID, u32, *mut PresetConfig) -> Status;
pub(super) type InitializeEncoder =
    unsafe extern "system" fn(Session, *mut InitializeParams) -> Status;
pub(super) type CreateBitstreamBufferFn =
    unsafe extern "system" fn(Session, *mut CreateBitstreamBuffer) -> Status;
pub(super) type WithPointer = unsafe extern "system" fn(Session, *mut c_void) -> Status;
pub(super) type EncodePicture = unsafe extern "system" fn(Session, *mut PicParams) -> Status;
pub(super) type LockBitstreamFn = unsafe extern "system" fn(Session, *mut LockBitstream) -> Status;
pub(super) type MapInputResourceFn =
    unsafe extern "system" fn(Session, *mut MapInputResource) -> Status;
pub(super) type RegisterResourceFn =
    unsafe extern "system" fn(Session, *mut RegisterResource) -> Status;
pub(super) type EventFn = unsafe extern "system" fn(Session, *mut EventParams) -> Status;
pub(super) type DestroyEncoder = unsafe extern "system" fn(Session) -> Status;
pub(super) type GetLastErrorString = unsafe extern "system" fn(Session) -> *const c_char;

/// `NV_ENCODE_API_FUNCTION_LIST`, filled in by `NvEncodeAPICreateInstance`.
#[repr(C)]
pub(super) struct FunctionList {
    pub(super) version: u32,
    _reserved: u32,
    _open_encode_session: Entry,
    pub(super) get_encode_guid_count: Option<GetEncodeGuidCount>,
    _profile_guids: [Entry; 2],
    pub(super) get_encode_guids: Option<GetEncodeGuids>,
    _input_formats: [Entry; 2],
    pub(super) get_encode_caps: Option<GetEncodeCaps>,
    _preset_queries: [Entry; 3],
    pub(super) initialize_encoder: Option<InitializeEncoder>,
    _input_buffers: [Entry; 2],
    pub(super) create_bitstream_buffer: Option<CreateBitstreamBufferFn>,
    pub(super) destroy_bitstream_buffer: Option<WithPointer>,
    pub(super) encode_picture: Option<EncodePicture>,
    pub(super) lock_bitstream: Option<LockBitstreamFn>,
    pub(super) unlock_bitstream: Option<WithPointer>,
    _input_locks_to_sequence_params: [Entry; 4],
    pub(super) register_async_event: Option<EventFn>,
    pub(super) unregister_async_event: Option<EventFn>,
    pub(super) map_input_resource: Option<MapInputResourceFn>,
    pub(super) unmap_input_resource: Option<WithPointer>,
    pub(super) destroy_encoder: Option<DestroyEncoder>,
    _invalidate_ref_frames: Entry,
    pub(super) open_encode_session_ex: Option<OpenEncodeSessionEx>,
    pub(super) register_resource: Option<RegisterResourceFn>,
    pub(super) unregister_resource: Option<WithPointer>,
    _reconfigure_to_motion_estimation: [Entry; 5],
    pub(super) get_last_error_string: Option<GetLastErrorString>,
    _set_io_cuda_streams: Entry,
    pub(super) get_encode_preset_config_ex: Option<GetEncodePresetConfigEx>,
    _rest: [Entry; 3 + 275],
}

// SAFETY: pointers and optional function pointers only.
unsafe impl Zeroed for FunctionList {}

const _: () = assert!(size_of::<FunctionList>() == 2552);
const _: () = assert!(offset_of!(FunctionList, get_encode_guids) == 40);
const _: () = assert!(offset_of!(FunctionList, get_encode_caps) == 64);
const _: () = assert!(offset_of!(FunctionList, initialize_encoder) == 96);
const _: () = assert!(offset_of!(FunctionList, create_bitstream_buffer) == 120);
const _: () = assert!(offset_of!(FunctionList, encode_picture) == 136);
const _: () = assert!(offset_of!(FunctionList, register_async_event) == 192);
const _: () = assert!(offset_of!(FunctionList, map_input_resource) == 208);
const _: () = assert!(offset_of!(FunctionList, destroy_encoder) == 224);
const _: () = assert!(offset_of!(FunctionList, open_encode_session_ex) == 240);
const _: () = assert!(offset_of!(FunctionList, register_resource) == 248);
const _: () = assert!(offset_of!(FunctionList, get_last_error_string) == 304);
const _: () = assert!(offset_of!(FunctionList, get_encode_preset_config_ex) == 320);

/// `NV_ENC_CAPS_PARAM`.
#[repr(C)]
pub(super) struct CapsParam {
    pub(super) version: u32,
    pub(super) caps_to_query: u32,
    _reserved: [u32; 62],
}

// SAFETY: integers only.
unsafe impl Zeroed for CapsParam {}

const _: () = assert!(size_of::<CapsParam>() == 256);

/// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS`.
#[repr(C)]
pub(super) struct OpenEncodeSessionExParams {
    pub(super) version: u32,
    pub(super) device_type: u32,
    pub(super) device: *mut c_void,
    _reserved: *mut c_void,
    pub(super) api_version: u32,
    _reserved1: [u32; 253],
    _reserved2: [*mut c_void; 64],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for OpenEncodeSessionExParams {}

const _: () = assert!(size_of::<OpenEncodeSessionExParams>() == 1552);
const _: () = assert!(offset_of!(OpenEncodeSessionExParams, api_version) == 24);

/// `NV_ENC_RC_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct RcParams {
    pub(super) version: u32,
    pub(super) rate_control_mode: u32,
    _const_qp: [u32; 3],
    pub(super) average_bit_rate: u32,
    pub(super) max_bit_rate: u32,
    _vbv: [u32; 2],
    /// Bit fields, `enableMinQP` first; see [`RC_ENABLE_LOOKAHEAD`].
    pub(super) flags: u32,
    _qp_to_qp_map_mode: [u32; 15],
    pub(super) multi_pass: u32,
    _rest: [u32; 6],
}

const _: () = assert!(size_of::<RcParams>() == 128);
const _: () = assert!(offset_of!(RcParams, average_bit_rate) == 20);
const _: () = assert!(offset_of!(RcParams, flags) == 36);
const _: () = assert!(offset_of!(RcParams, multi_pass) == 100);

/// `NV_ENC_CONFIG_H264_VUI_PARAMETERS`, which HEVC shares.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Vui {
    _overscan: [u32; 2],
    pub(super) video_signal_type_present_flag: u32,
    pub(super) video_format: u32,
    pub(super) video_full_range_flag: u32,
    pub(super) colour_description_present_flag: u32,
    pub(super) colour_primaries: u32,
    pub(super) transfer_characteristics: u32,
    pub(super) colour_matrix: u32,
    _chroma_location_to_time_scale: [u32; 7],
    _reserved: [u32; 12],
}

const _: () = assert!(size_of::<Vui>() == 112);

impl Vui {
    /// Limited-range BT.709, as the RGB-to-YUV conversion produces and players assume for HD.
    pub(super) fn signal_bt709(&mut self) {
        self.video_signal_type_present_flag = 1;
        self.video_format = VIDEO_FORMAT_UNSPECIFIED;
        self.video_full_range_flag = 0;
        self.colour_description_present_flag = 1;
        self.colour_primaries = BT709;
        self.transfer_characteristics = BT709;
        self.colour_matrix = BT709;
    }
}

/// `NV_ENC_CONFIG_H264`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct H264Config {
    /// Bit fields, `enableTemporalSVC` first; see [`H264_REPEAT_SPS_PPS`].
    pub(super) flags: u32,
    pub(super) level: u32,
    pub(super) idr_period: u32,
    _colour_plane_to_slice_mode: [u32; 15],
    pub(super) vui: Vui,
    _ltr: [u32; 2],
    pub(super) chroma_format_idc: u32,
    _rest: [u32; 4 + 267],
    _reserved2: [*mut c_void; 64],
}

const _: () = assert!(size_of::<H264Config>() == 1792);
const _: () = assert!(offset_of!(H264Config, idr_period) == 8);
const _: () = assert!(offset_of!(H264Config, vui) == 72);
const _: () = assert!(offset_of!(H264Config, chroma_format_idc) == 192);

/// `NV_ENC_CONFIG_HEVC`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct HevcConfig {
    _level_to_cu_sizes: [u32; 4],
    /// Bit fields, `useConstrainedIntraPred` first; see [`HEVC_REPEAT_SPS_PPS`].
    pub(super) flags: u32,
    pub(super) idr_period: u32,
    _intra_refresh_to_temporal_layers: [u32; 10],
    pub(super) vui: Vui,
    _rest: [u32; 4 + 214],
    _reserved2: [*mut c_void; 64],
}

const _: () = assert!(size_of::<HevcConfig>() == 1560);
const _: () = assert!(offset_of!(HevcConfig, idr_period) == 20);
const _: () = assert!(offset_of!(HevcConfig, vui) == 64);

/// `NV_ENC_CONFIG_AV1`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Av1Config {
    _level_to_part_sizes: [u32; 4],
    /// Bit fields, `outputAnnexBFormat` first; see [`AV1_REPEAT_SEQUENCE_HEADER`].
    pub(super) flags: u32,
    pub(super) idr_period: u32,
    _intra_refresh_to_tile_rows: [u32; 5],
    _tile_sizes: [*mut u32; 2],
    _max_temporal_layers: u32,
    pub(super) color_primaries: u32,
    pub(super) transfer_characteristics: u32,
    pub(super) matrix_coefficients: u32,
    pub(super) color_range: u32,
    _chroma_sample_position: u32,
    _use_b_frames_as_ref: u32,
    _film_grain_params: *mut c_void,
    _references: [u32; 2],
    _reserved1: [u32; 235],
    _reserved2: [*mut c_void; 62],
}

const _: () = assert!(size_of::<Av1Config>() == 1552);
const _: () = assert!(offset_of!(Av1Config, idr_period) == 20);
const _: () = assert!(offset_of!(Av1Config, color_primaries) == 68);
const _: () = assert!(offset_of!(Av1Config, color_range) == 80);

/// `NV_ENC_CODEC_CONFIG`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union CodecConfig {
    pub(super) h264: H264Config,
    pub(super) hevc: HevcConfig,
    pub(super) av1: Av1Config,
}

const _: () = assert!(size_of::<CodecConfig>() == 1792);

/// `NV_ENC_CONFIG`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Config {
    pub(super) version: u32,
    pub(super) profile_guid: GUID,
    pub(super) gop_length: u32,
    pub(super) frame_interval_p: i32,
    _monochrome_field_mode_mv_precision: [u32; 3],
    pub(super) rc: RcParams,
    pub(super) codec: CodecConfig,
    _reserved: [u32; 278],
    _reserved2: [*mut c_void; 64],
}

const _: () = assert!(size_of::<Config>() == 3584);
const _: () = assert!(offset_of!(Config, gop_length) == 20);
const _: () = assert!(offset_of!(Config, frame_interval_p) == 24);
const _: () = assert!(offset_of!(Config, rc) == 40);
const _: () = assert!(offset_of!(Config, codec) == 168);

/// `NV_ENC_PRESET_CONFIG`.
#[repr(C)]
pub(super) struct PresetConfig {
    pub(super) version: u32,
    pub(super) config: Config,
    _reserved1: [u32; 255],
    _reserved2: [*mut c_void; 64],
}

// SAFETY: integers, GUIDs, pointers and unions of those only.
unsafe impl Zeroed for PresetConfig {}

const _: () = assert!(size_of::<PresetConfig>() == 5128);
const _: () = assert!(offset_of!(PresetConfig, config) == 8);

/// `NV_ENC_INITIALIZE_PARAMS`.
#[repr(C)]
pub(super) struct InitializeParams {
    pub(super) version: u32,
    pub(super) encode_guid: GUID,
    pub(super) preset_guid: GUID,
    pub(super) encode_width: u32,
    pub(super) encode_height: u32,
    pub(super) dar_width: u32,
    pub(super) dar_height: u32,
    pub(super) frame_rate_num: u32,
    pub(super) frame_rate_den: u32,
    pub(super) enable_encode_async: u32,
    pub(super) enable_ptd: u32,
    _flags: u32,
    _private_data_size: u32,
    _private_data: *mut c_void,
    pub(super) encode_config: *mut Config,
    pub(super) max_encode_width: u32,
    pub(super) max_encode_height: u32,
    _max_me_hint_counts: [u32; 8],
    pub(super) tuning_info: u32,
    _buffer_format_to_stats_level: [u32; 3],
    _reserved: [u32; 285],
    _reserved2: [*mut c_void; 64],
}

// SAFETY: integers, GUIDs and pointers only.
unsafe impl Zeroed for InitializeParams {}

const _: () = assert!(size_of::<InitializeParams>() == 1808);
const _: () = assert!(offset_of!(InitializeParams, encode_width) == 36);
const _: () = assert!(offset_of!(InitializeParams, enable_ptd) == 64);
const _: () = assert!(offset_of!(InitializeParams, encode_config) == 88);
const _: () = assert!(offset_of!(InitializeParams, max_encode_width) == 96);
const _: () = assert!(offset_of!(InitializeParams, tuning_info) == 136);

/// `NV_ENC_REGISTER_RESOURCE`.
#[repr(C)]
pub(super) struct RegisterResource {
    pub(super) version: u32,
    pub(super) resource_type: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    _pitch: u32,
    _sub_resource_index: u32,
    pub(super) resource_to_register: *mut c_void,
    pub(super) registered_resource: *mut c_void,
    pub(super) buffer_format: u32,
    pub(super) buffer_usage: u32,
    _input_fence_point: *mut c_void,
    _rest: [u64; 185],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for RegisterResource {}

const _: () = assert!(size_of::<RegisterResource>() == 1536);
const _: () = assert!(offset_of!(RegisterResource, resource_to_register) == 24);
const _: () = assert!(offset_of!(RegisterResource, buffer_format) == 40);

/// `NV_ENC_CREATE_BITSTREAM_BUFFER`.
#[repr(C)]
pub(super) struct CreateBitstreamBuffer {
    pub(super) version: u32,
    _size_heap_reserved: [u32; 3],
    pub(super) bitstream_buffer: *mut c_void,
    _rest: [u64; 94],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for CreateBitstreamBuffer {}

const _: () = assert!(size_of::<CreateBitstreamBuffer>() == 776);
const _: () = assert!(offset_of!(CreateBitstreamBuffer, bitstream_buffer) == 16);

/// `NV_ENC_EVENT_PARAMS`.
#[repr(C)]
pub(super) struct EventParams {
    pub(super) version: u32,
    _reserved: u32,
    pub(super) completion_event: *mut c_void,
    _rest: [u64; 191],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for EventParams {}

const _: () = assert!(size_of::<EventParams>() == 1544);

/// `NV_ENC_MAP_INPUT_RESOURCE`.
#[repr(C)]
pub(super) struct MapInputResource {
    pub(super) version: u32,
    _sub_resource_index: u32,
    _input_resource: *mut c_void,
    pub(super) registered_resource: *mut c_void,
    pub(super) mapped_resource: *mut c_void,
    pub(super) mapped_buffer_format: u32,
    _rest: [u32; 377],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for MapInputResource {}

const _: () = assert!(size_of::<MapInputResource>() == 1544);
const _: () = assert!(offset_of!(MapInputResource, mapped_resource) == 24);
const _: () = assert!(offset_of!(MapInputResource, mapped_buffer_format) == 32);

/// `NV_ENC_PIC_PARAMS`.
#[repr(C)]
pub(super) struct PicParams {
    pub(super) version: u32,
    pub(super) input_width: u32,
    pub(super) input_height: u32,
    _input_pitch: u32,
    pub(super) encode_pic_flags: u32,
    _frame_index: u32,
    pub(super) input_time_stamp: u64,
    _input_duration: u64,
    pub(super) input_buffer: *mut c_void,
    pub(super) output_bitstream: *mut c_void,
    pub(super) completion_event: *mut c_void,
    pub(super) buffer_format: u32,
    pub(super) picture_struct: u32,
    _picture_type: u32,
    _rest: [u32; 821],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for PicParams {}

const _: () = assert!(size_of::<PicParams>() == 3360);
const _: () = assert!(offset_of!(PicParams, input_time_stamp) == 24);
const _: () = assert!(offset_of!(PicParams, input_buffer) == 40);
const _: () = assert!(offset_of!(PicParams, completion_event) == 56);
const _: () = assert!(offset_of!(PicParams, buffer_format) == 64);

/// `NV_ENC_LOCK_BITSTREAM`.
#[repr(C)]
pub(super) struct LockBitstream {
    pub(super) version: u32,
    _flags: u32,
    pub(super) output_bitstream: *mut c_void,
    _slice_offsets: *mut u32,
    _frame_index_to_num_slices: [u32; 3],
    pub(super) bitstream_size_in_bytes: u32,
    pub(super) output_time_stamp: u64,
    _output_duration: u64,
    pub(super) bitstream_buffer_ptr: *mut c_void,
    pub(super) picture_type: u32,
    _rest: [u32; 371],
}

// SAFETY: integers and pointers only.
unsafe impl Zeroed for LockBitstream {}

const _: () = assert!(size_of::<LockBitstream>() == 1552);
const _: () = assert!(offset_of!(LockBitstream, bitstream_size_in_bytes) == 36);
const _: () = assert!(offset_of!(LockBitstream, output_time_stamp) == 40);
const _: () = assert!(offset_of!(LockBitstream, bitstream_buffer_ptr) == 56);
const _: () = assert!(offset_of!(LockBitstream, picture_type) == 64);
