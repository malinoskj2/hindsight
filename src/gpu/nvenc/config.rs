//! The encoder configuration: the preset with this program's edits, and the parameters a
//! session is initialized with.

use super::{Settings, ffi};
use crate::config::Codec;
use bytemuck::Zeroable;
use windows::core::GUID;

/// Two keyframes a second: a clip starts within half a second of where it is asked to, and
/// the rest of the bitrate goes to the P-frames. At 60 fps that is Instant Replay's 30 frames.
const KEYFRAMES_PER_SECOND: u32 = 2;

pub(super) const fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::CODEC_H264,
        Codec::Hevc => ffi::CODEC_HEVC,
        Codec::Av1 => ffi::CODEC_AV1,
    }
}

const fn profile_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::PROFILE_H264_HIGH,
        Codec::Hevc => ffi::PROFILE_HEVC_MAIN,
        Codec::Av1 => ffi::PROFILE_AV1_MAIN,
    }
}

#[expect(unsafe_code, reason = "reads the codec union NVENC's preset filled")]
pub(super) fn configure(config: &mut ffi::Config, settings: Settings) {
    let matrix = u32::from(settings.matrix.code());
    let gop = gop_frames(settings.frame_rate);
    config.profile_guid = profile_guid(settings.codec);
    config.gop_length = gop;
    // No B-frames.
    config.frame_interval_p = 1;
    configure_rate_control(&mut config.rc, settings.bitrate_bps);
    match settings.codec {
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::H264 => configure_h264(unsafe { &mut config.codec.h264 }, gop, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Hevc => configure_hevc(unsafe { &mut config.codec.hevc }, gop, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Av1 => configure_av1(unsafe { &mut config.codec.av1 }, gop, matrix),
    }
}

/// Frames between keyframes, never fewer than one.
const fn gop_frames(frame_rate: u32) -> u32 {
    let frames = frame_rate / KEYFRAMES_PER_SECOND;
    if frames == 0 { 1 } else { frames }
}

/// Single-pass VBR with no lookahead or adaptive quantization: nothing runs on the shader cores.
fn configure_rate_control(rc: &mut ffi::RcParams, bitrate_bps: u32) {
    rc.rate_control_mode = ffi::RATE_CONTROL_VBR;
    rc.average_bit_rate = bitrate_bps;
    rc.max_bit_rate = bitrate_bps.saturating_mul(2);
    rc.vbv_buffer_size = rc.max_bit_rate;
    rc.flags = 0;
    rc.multi_pass = ffi::MULTI_PASS_DISABLED;
}

fn configure_h264(h264: &mut ffi::H264Config, gop: u32, matrix: u32) {
    h264.flags |= ffi::H264_REPEAT_SPS_PPS;
    h264.idr_period = gop;
    h264.chroma_format_idc = ffi::CHROMA_FORMAT_420;
    h264.vui.signal_colour(matrix);
}

/// Coding units held to 16×16–32×32 for speed.
fn configure_hevc(hevc: &mut ffi::HevcConfig, gop: u32, matrix: u32) {
    hevc.flags &= !(ffi::HEVC_CHROMA_FORMAT_MASK | ffi::HEVC_BIT_DEPTH_MASK);
    hevc.flags |=
        ffi::HEVC_REPEAT_SPS_PPS | (ffi::CHROMA_FORMAT_420 << ffi::HEVC_CHROMA_FORMAT_SHIFT);
    hevc.idr_period = gop;
    hevc.min_cu_size = ffi::HEVC_CU_SIZE_16;
    hevc.max_cu_size = ffi::HEVC_CU_SIZE_32;
    hevc.vui.signal_colour(matrix);
}

/// Partitions held to 16×16–32×32 for speed; plain OBUs without Annex B framing.
fn configure_av1(av1: &mut ffi::Av1Config, gop: u32, matrix: u32) {
    av1.flags &= !(ffi::AV1_ANNEX_B
        | ffi::AV1_TIMING_INFO
        | ffi::AV1_DECODER_MODEL_INFO
        | ffi::AV1_FRAME_ID_NUMBERS
        | ffi::AV1_DISABLE_SEQUENCE_HEADER
        | ffi::AV1_CHROMA_FORMAT_MASK
        | ffi::AV1_BIT_DEPTH_MASK);
    av1.flags |=
        ffi::AV1_REPEAT_SEQUENCE_HEADER | (ffi::CHROMA_FORMAT_420 << ffi::AV1_CHROMA_FORMAT_SHIFT);
    av1.idr_period = gop;
    av1.min_part_size = ffi::AV1_PART_SIZE_16;
    av1.max_part_size = ffi::AV1_PART_SIZE_32;
    av1.signal_colour(matrix);
}

/// Without the configuration, which [`Session::initialize`](super::session::Session::initialize)
/// points it at.
pub(super) fn initialize_params(settings: Settings) -> ffi::InitializeParams {
    let mut params = ffi::InitializeParams::zeroed();
    params.version = ffi::INITIALIZE_PARAMS_VER;
    params.encode_guid = codec_guid(settings.codec);
    params.preset_guid = ffi::PRESET_P4;
    params.encode_width = settings.width;
    params.encode_height = settings.height;
    params.dar_width = settings.width;
    params.dar_height = settings.height;
    params.frame_rate_num = settings.frame_rate;
    params.frame_rate_den = 1;
    params.enable_encode_async = 1;
    params.enable_ptd = 1;
    params.max_encode_width = settings.width;
    params.max_encode_height = settings.height;
    params.tuning_info = ffi::TUNING_HIGH_QUALITY;
    params
}

#[cfg(test)]
#[expect(unsafe_code, reason = "the tests read the codec union")]
mod tests {
    use super::*;
    use crate::replay::Matrix;

    fn settings(codec: Codec) -> Settings {
        Settings {
            codec,
            width: 1920,
            height: 1080,
            frame_rate: 60,
            bitrate_bps: 20_000_000,
            matrix: Matrix::Bt709,
        }
    }

    /// A preset as the driver might hand it back: every bit field set, so the test sees which
    /// bits the edits clear, set and leave alone.
    fn preset(codec: Codec) -> ffi::Config {
        let mut config = ffi::PresetConfig::zeroed().config;
        match codec {
            Codec::H264 => config.codec.h264.flags = u32::MAX,
            Codec::Hevc => config.codec.hevc.flags = u32::MAX,
            Codec::Av1 => config.codec.av1.flags = u32::MAX,
        }
        config
    }

    fn configured(codec: Codec) -> ffi::Config {
        let mut config = preset(codec);
        configure(&mut config, settings(codec));
        config
    }

    #[test]
    fn keyframes_come_twice_a_second_whatever_the_frame_rate() {
        assert_eq!(gop_frames(60), 30);
        assert_eq!(gop_frames(240), 120);
        assert_eq!(gop_frames(25), 12);
        assert_eq!(gop_frames(1), 1);
    }

    #[test]
    fn every_codec_gets_the_gop_no_b_frames_and_single_pass_vbr() {
        for codec in Codec::ALL {
            let config = configured(codec);
            assert_eq!(config.profile_guid, profile_guid(codec));
            assert_eq!(config.gop_length, 30);
            assert_eq!(config.frame_interval_p, 1);
            assert_eq!(config.rc.rate_control_mode, ffi::RATE_CONTROL_VBR);
            assert_eq!(config.rc.average_bit_rate, 20_000_000);
            assert_eq!(config.rc.max_bit_rate, 40_000_000);
            assert_eq!(config.rc.vbv_buffer_size, 40_000_000);
            assert_eq!(config.rc.flags, 0);
            assert_eq!(config.rc.multi_pass, ffi::MULTI_PASS_DISABLED);
        }
    }

    #[test]
    fn the_peak_bitrate_saturates() {
        let mut rc = ffi::PresetConfig::zeroed().config.rc;
        configure_rate_control(&mut rc, u32::MAX);
        assert_eq!(rc.max_bit_rate, u32::MAX);
    }

    #[test]
    fn h264_repeats_its_parameter_sets_in_4_2_0() {
        // SAFETY: the H.264 member is the one `configure` filled.
        let h264 = unsafe { configured(Codec::H264).codec.h264 };
        assert_eq!(h264.flags, u32::MAX, "only sets bits, here already set");
        assert_eq!(h264.idr_period, 30);
        assert_eq!(h264.chroma_format_idc, ffi::CHROMA_FORMAT_420);

        let mut h264 = ffi::PresetConfig::zeroed().config.codec;
        // SAFETY: the union is all zeros, a valid H.264 member.
        let h264 = unsafe { &mut h264.h264 };
        configure_h264(h264, 30, 1);
        assert_eq!(h264.flags, ffi::H264_REPEAT_SPS_PPS);
    }

    #[test]
    fn hevc_is_8_bit_4_2_0_with_its_parameter_sets_repeated() {
        // SAFETY: the HEVC member is the one `configure` filled.
        let hevc = unsafe { configured(Codec::Hevc).codec.hevc };
        let kept = !(ffi::HEVC_CHROMA_FORMAT_MASK | ffi::HEVC_BIT_DEPTH_MASK);
        assert_eq!(hevc.flags & kept, kept, "bits outside the edits are kept");
        assert_eq!(
            hevc.flags & ffi::HEVC_CHROMA_FORMAT_MASK,
            ffi::CHROMA_FORMAT_420 << ffi::HEVC_CHROMA_FORMAT_SHIFT
        );
        assert_eq!(hevc.flags & ffi::HEVC_BIT_DEPTH_MASK, 0, "8-bit");
        assert_ne!(hevc.flags & ffi::HEVC_REPEAT_SPS_PPS, 0);
        assert_eq!(hevc.idr_period, 30);
        assert_eq!(
            (hevc.min_cu_size, hevc.max_cu_size),
            (ffi::HEVC_CU_SIZE_16, ffi::HEVC_CU_SIZE_32)
        );
    }

    #[test]
    fn av1_is_plain_8_bit_4_2_0_obus_with_the_sequence_header_repeated() {
        // SAFETY: the AV1 member is the one `configure` filled.
        let av1 = unsafe { configured(Codec::Av1).codec.av1 };
        let cleared = ffi::AV1_ANNEX_B
            | ffi::AV1_TIMING_INFO
            | ffi::AV1_DECODER_MODEL_INFO
            | ffi::AV1_FRAME_ID_NUMBERS
            | ffi::AV1_DISABLE_SEQUENCE_HEADER;
        assert_eq!(av1.flags & cleared, 0);
        assert_ne!(av1.flags & ffi::AV1_REPEAT_SEQUENCE_HEADER, 0);
        assert_eq!(
            av1.flags & ffi::AV1_CHROMA_FORMAT_MASK,
            ffi::CHROMA_FORMAT_420 << ffi::AV1_CHROMA_FORMAT_SHIFT
        );
        assert_eq!(av1.flags & ffi::AV1_BIT_DEPTH_MASK, 0, "8-bit");
        let kept = !(cleared | ffi::AV1_CHROMA_FORMAT_MASK | ffi::AV1_BIT_DEPTH_MASK);
        assert_eq!(av1.flags & kept, kept, "bits outside the edits are kept");
        assert_eq!(av1.idr_period, 30);
        assert_eq!(
            (av1.min_part_size, av1.max_part_size),
            (ffi::AV1_PART_SIZE_16, ffi::AV1_PART_SIZE_32)
        );
    }

    #[test]
    fn the_session_is_initialized_for_the_settings() {
        let params = initialize_params(settings(Codec::Hevc));
        assert_eq!(params.encode_guid, ffi::CODEC_HEVC);
        assert_eq!(params.preset_guid, ffi::PRESET_P4);
        assert_eq!((params.encode_width, params.encode_height), (1920, 1080));
        assert_eq!((params.frame_rate_num, params.frame_rate_den), (60, 1));
        assert!(
            params.encode_config.is_null(),
            "pointed at only when initializing"
        );
    }
}
