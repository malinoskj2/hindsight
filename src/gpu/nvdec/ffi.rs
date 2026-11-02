//! The parts of `cuviddec.h` and `nvcuvid.h` (Video Codec SDK 12) this program uses. Every
//! layout is checked at compile time against the 64-bit Windows headers, where `unsigned long`
//! is 32 bits.

use crate::gpu::cuda::Status;
use bytemuck::Zeroable;
use std::{
    ffi::c_void,
    mem::{offset_of, size_of},
};

/// `cudaVideoCodec`.
pub(super) const CODEC_H264: u32 = 4;
pub(super) const CODEC_HEVC: u32 = 8;
pub(super) const CODEC_AV1: u32 = 11;
/// `cudaVideoChromaFormat_420`.
pub(super) const CHROMA_420: u32 = 1;
/// `cudaVideoSurfaceFormat_NV12`.
pub(super) const SURFACE_NV12: u32 = 0;
/// `cudaVideoDeinterlaceMode_Weave` and `_Adaptive`.
pub(super) const DEINTERLACE_WEAVE: u32 = 0;
pub(super) const DEINTERLACE_ADAPTIVE: u32 = 2;
/// `cudaVideoCreate_PreferCUVID`: decode on the NVDEC engine.
pub(super) const CREATE_PREFER_CUVID: u32 = 4;
/// `CUVID_PKT_ENDOFSTREAM`, `CUVID_PKT_TIMESTAMP` and `CUVID_PKT_ENDOFPICTURE`.
pub(super) const PACKET_END_OF_STREAM: u32 = 0x01;
pub(super) const PACKET_TIMESTAMP: u32 = 0x02;
pub(super) const PACKET_END_OF_PICTURE: u32 = 0x08;

pub(super) type Parser = *mut c_void;
pub(super) type RawDecoder = *mut c_void;

/// `CUVIDEOFORMAT`, which the parser hands the sequence callback.
#[repr(C)]
#[derive(Clone, Copy, Zeroable)]
pub(super) struct VideoFormat {
    pub(super) codec: u32,
    pub(super) frame_rate: [u32; 2],
    pub(super) progressive_sequence: u8,
    pub(super) bit_depth_luma_minus8: u8,
    pub(super) bit_depth_chroma_minus8: u8,
    pub(super) min_num_decode_surfaces: u8,
    pub(super) coded_width: u32,
    pub(super) coded_height: u32,
    pub(super) display_area: Area<i32>,
    pub(super) chroma_format: u32,
    pub(super) bitrate: u32,
    pub(super) display_aspect_ratio: [i32; 2],
    pub(super) video_signal_description: [u8; 4],
    pub(super) seqhdr_data_length: u32,
}

const _: () = assert!(size_of::<VideoFormat>() == 64);
const _: () = assert!(offset_of!(VideoFormat, coded_width) == 16);
const _: () = assert!(offset_of!(VideoFormat, display_area) == 24);
const _: () = assert!(offset_of!(VideoFormat, chroma_format) == 40);
const _: () = assert!(offset_of!(VideoFormat, seqhdr_data_length) == 60);

#[repr(C)]
#[derive(Clone, Copy, Zeroable)]
pub(super) struct Area<T> {
    pub(super) left: T,
    pub(super) top: T,
    pub(super) right: T,
    pub(super) bottom: T,
}

/// `CUVIDPARSERDISPINFO`.
#[repr(C)]
#[derive(Clone, Copy, Zeroable)]
pub(super) struct DisplayInfo {
    pub(super) picture_index: i32,
    pub(super) progressive_frame: i32,
    pub(super) top_field_first: i32,
    pub(super) repeat_first_field: i32,
    pub(super) timestamp: i64,
}

const _: () = assert!(size_of::<DisplayInfo>() == 24);

/// `CUVIDPICPARAMS`, which passes from the parser to the decoder untouched.
pub(super) type PictureParams = c_void;

pub(super) type SequenceCallback = unsafe extern "system" fn(*mut c_void, *mut VideoFormat) -> i32;
pub(super) type DecodeCallback = unsafe extern "system" fn(*mut c_void, *mut PictureParams) -> i32;
pub(super) type DisplayCallback = unsafe extern "system" fn(*mut c_void, *mut DisplayInfo) -> i32;

/// `CUVIDPARSERPARAMS`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct ParserParams {
    pub(super) codec: u32,
    pub(super) max_num_decode_surfaces: u32,
    pub(super) clock_rate: u32,
    pub(super) error_threshold: u32,
    pub(super) max_display_delay: u32,
    /// `bAnnexb`, `bMemoryOptimize` and reserved bits.
    pub(super) flags: u32,
    _reserved1: [u32; 4],
    pub(super) user_data: *mut c_void,
    pub(super) sequence_callback: Option<SequenceCallback>,
    pub(super) decode_picture: Option<DecodeCallback>,
    pub(super) display_picture: Option<DisplayCallback>,
    _get_operating_point: *mut c_void,
    _get_sei_message: *mut c_void,
    _reserved2: [*mut c_void; 5],
    _ext_video_info: *mut c_void,
}

const _: () = assert!(size_of::<ParserParams>() == 136);
const _: () = assert!(offset_of!(ParserParams, user_data) == 40);
const _: () = assert!(offset_of!(ParserParams, display_picture) == 64);

/// `CUVIDSOURCEDATAPACKET`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct SourcePacket {
    pub(super) flags: u32,
    pub(super) payload_size: u32,
    pub(super) payload: *const u8,
    pub(super) timestamp: i64,
}

const _: () = assert!(size_of::<SourcePacket>() == 24);
const _: () = assert!(offset_of!(SourcePacket, timestamp) == 16);

/// `CUVIDDECODECAPS`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct DecoderCaps {
    pub(super) codec: u32,
    pub(super) chroma_format: u32,
    pub(super) bit_depth_minus8: u32,
    _reserved1: [u32; 3],
    pub(super) is_supported: u8,
    pub(super) num_nvdecs: u8,
    pub(super) output_format_mask: u16,
    pub(super) max_width: u32,
    pub(super) max_height: u32,
    pub(super) max_mb_count: u32,
    pub(super) min_width: u16,
    pub(super) min_height: u16,
    _histogram: [u8; 4],
    _reserved3: [u32; 10],
}

const _: () = assert!(size_of::<DecoderCaps>() == 88);
const _: () = assert!(offset_of!(DecoderCaps, is_supported) == 24);
const _: () = assert!(offset_of!(DecoderCaps, max_width) == 28);
const _: () = assert!(offset_of!(DecoderCaps, min_width) == 40);

/// `CUVIDDECODECREATEINFO`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct CreateInfo {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) num_decode_surfaces: u32,
    pub(super) codec: u32,
    pub(super) chroma_format: u32,
    pub(super) creation_flags: u32,
    pub(super) bit_depth_minus8: u32,
    pub(super) intra_decode_only: u32,
    pub(super) max_width: u32,
    pub(super) max_height: u32,
    _reserved1: u32,
    pub(super) display_area: Area<i16>,
    pub(super) output_format: u32,
    pub(super) deinterlace_mode: u32,
    pub(super) target_width: u32,
    pub(super) target_height: u32,
    pub(super) num_output_surfaces: u32,
    pub(super) context_lock: *mut c_void,
    pub(super) target_rect: Area<i16>,
    pub(super) enable_histogram: u32,
    _reserved2: [u32; 4],
}

const _: () = assert!(size_of::<CreateInfo>() == 112);
const _: () = assert!(offset_of!(CreateInfo, display_area) == 44);
const _: () = assert!(offset_of!(CreateInfo, output_format) == 52);
const _: () = assert!(offset_of!(CreateInfo, num_output_surfaces) == 68);
const _: () = assert!(offset_of!(CreateInfo, context_lock) == 72);
const _: () = assert!(offset_of!(CreateInfo, target_rect) == 80);

/// `CUVIDPROCPARAMS`.
#[repr(C)]
#[derive(Zeroable)]
pub(super) struct ProcessingParams {
    pub(super) progressive_frame: i32,
    pub(super) second_field: i32,
    pub(super) top_field_first: i32,
    pub(super) unpaired_field: i32,
    _reserved_flags: u32,
    _reserved_zero: u32,
    _raw_input: u64,
    _raw_input_pitch: u32,
    _raw_input_format: u32,
    _raw_output: u64,
    _raw_output_pitch: u32,
    _reserved1: u32,
    pub(super) output_stream: *mut c_void,
    _reserved: [u32; 46],
    _histogram: *mut c_void,
    _reserved2: *mut c_void,
}

const _: () = assert!(size_of::<ProcessingParams>() == 264);
const _: () = assert!(offset_of!(ProcessingParams, output_stream) == 56);

pub(super) type CreateVideoParser =
    unsafe extern "system" fn(*mut Parser, *mut ParserParams) -> Status;
pub(super) type ParseVideoData = unsafe extern "system" fn(Parser, *mut SourcePacket) -> Status;
pub(super) type DestroyVideoParser = unsafe extern "system" fn(Parser) -> Status;
pub(super) type GetDecoderCaps = unsafe extern "system" fn(*mut DecoderCaps) -> Status;
pub(super) type CreateDecoder =
    unsafe extern "system" fn(*mut RawDecoder, *mut CreateInfo) -> Status;
pub(super) type DestroyDecoder = unsafe extern "system" fn(RawDecoder) -> Status;
pub(super) type DecodePicture = unsafe extern "system" fn(RawDecoder, *mut PictureParams) -> Status;
pub(super) type MapVideoFrame =
    unsafe extern "system" fn(RawDecoder, i32, *mut u64, *mut u32, *mut ProcessingParams) -> Status;
pub(super) type UnmapVideoFrame = unsafe extern "system" fn(RawDecoder, u64) -> Status;
