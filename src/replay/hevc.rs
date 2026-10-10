//! HEVC in MP4 (ISO/IEC 14496-15): length-prefixed NAL units in each sample, with the video,
//! sequence and picture parameter sets in an `hvcC` record.

use super::{
    annexb::{nal_units, unescape},
    mp4::{NAL_LENGTH_BYTES, atom, write_parameter_set},
};
use std::ops::{Range, RangeInclusive};

const NAL_TYPE_MASK: u8 = 0x3F;
const NAL_HEADER_BYTES: usize = 2;
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
pub(super) const NAL_AUD: u8 = 35;
/// Intra random access points: BLA, IDR and CRA pictures.
const IRAP: RangeInclusive<u8> = 16..=21;
/// Bytes of an SPS up to and including `general_level_idc`, after emulation prevention.
const SPS_PROFILE_BYTES: usize = 15;
/// Where `profile_tier_level` sits in those bytes: general profile space, tier and profile,
/// compatibility flags, constraint flags and level, which the `hvcC` record copies as they are.
const SPS_PROFILE_TIER_LEVEL: Range<usize> = 3..SPS_PROFILE_BYTES;
/// `configurationVersion` of the `hvcC` record.
const HVCC_VERSION: u8 = 1;
/// The `array_completeness` flag: every parameter set of the type is in the record, none only in
/// the samples.
const ARRAY_COMPLETE: u8 = 0x80;

/// Whether the access unit holds an intra random access point picture.
pub(super) fn is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit)
        .iter()
        .any(|unit| IRAP.contains(&nal_type(unit)))
}

/// The `hvcC` record for the parameter sets in `access_unit`, if it carries all three.
pub(super) fn configuration(access_unit: &[u8]) -> Option<Vec<u8>> {
    let units = nal_units(access_unit);
    let of_type = |kind| units.iter().copied().find(|unit| nal_type(unit) == kind);
    let (vps, sps, pps) = (of_type(NAL_VPS)?, of_type(NAL_SPS)?, of_type(NAL_PPS)?);
    let mut body = record_fields(sps)?;
    // One array per parameter set type, each holding one parameter set behind its count.
    for unit in [vps, sps, pps] {
        body.push(ARRAY_COMPLETE | nal_type(unit));
        body.extend_from_slice(&1_u16.to_be_bytes());
        write_parameter_set(&mut body, unit)?;
    }
    Some(atom(*b"hvcC", &body))
}

/// The `hvcC` record's fields before its parameter set arrays: the profile, tier and level and
/// the sub-layers, copied from `sps`, and the stream format the encoder is configured for.
fn record_fields(sps: &[u8]) -> Option<Vec<u8>> {
    // After the two-byte NAL header: VPS id (4 bits), max sub-layers minus one (3) and temporal
    // id nesting (1), then `profile_tier_level`.
    let header = unescape(sps, SPS_PROFILE_BYTES);
    if header.len() < SPS_PROFILE_BYTES {
        return None;
    }
    let sub_layers = ((header[2] >> 1) & 0b111) + 1;
    let temporal_id_nested = header[2] & 1;

    // The reserved bits are set throughout.
    let mut body = vec![HVCC_VERSION];
    body.extend_from_slice(&header[SPS_PROFILE_TIER_LEVEL]);
    body.extend_from_slice(&[
        // No minimum spatial segmentation.
        0xF0,
        0x00,
        // Unknown parallelism.
        0xFC,
        // 4:2:0, 8-bit luma and chroma: what the encoder is configured for.
        0xFC | 1,
        0xF8,
        0xF8,
        // Unknown average frame rate.
        0x00,
        0x00,
        // Frame rate not constant, the sub-layers, temporal id nesting, then the length size.
        (sub_layers << 3) | (temporal_id_nested << 2) | (NAL_LENGTH_BYTES - 1),
        // One array per parameter set type.
        3,
    ]);
    Some(body)
}

/// The NAL units a stored sample keeps: parameter sets travel in the sample description and
/// access unit delimiters carry nothing an MP4 reader needs.
pub(super) fn sample_units(access_unit: &[u8]) -> Vec<&[u8]> {
    nal_units(access_unit)
        .into_iter()
        .filter(|unit| unit.len() >= NAL_HEADER_BYTES)
        .filter(|unit| !matches!(nal_type(unit), NAL_VPS | NAL_SPS | NAL_PPS | NAL_AUD))
        .collect()
}

/// The `nal_unit_type` in the first byte of a NAL unit's two-byte header.
pub(super) fn nal_type(unit: &[u8]) -> u8 {
    (unit[0] >> 1) & NAL_TYPE_MASK
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::replay) const VPS: [u8; 6] = [0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF];
    /// Main profile, level 4.1, one sub-layer, with an emulation prevention byte in the
    /// constraint flags.
    pub(in crate::replay) const SPS: [u8; 18] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x7B,
    ];
    pub(in crate::replay) const PPS: [u8; 4] = [0x44, 0x01, 0xC1, 0x72];
    pub(in crate::replay) const IDR: [u8; 4] = [0x26, 0x01, 0xAF, 0x09];
    pub(in crate::replay) const TRAIL: [u8; 4] = [0x02, 0x01, 0xD0, 0x11];

    /// A keyframe access unit as NVENC writes it: delimiter, parameter sets, then the picture.
    pub(in crate::replay) fn keyframe() -> Vec<u8> {
        [
            &[0, 0, 0, 1, 0x46, 0x01, 0x10][..],
            &[0, 0, 0, 1],
            &VPS,
            &[0, 0, 0, 1],
            &SPS,
            &[0, 0, 0, 1],
            &PPS,
            &[0, 0, 0, 1],
            &IDR,
        ]
        .concat()
    }

    /// An access unit holding a delimiter and one trailing picture that refers to earlier ones.
    pub(in crate::replay) fn inter_frame() -> Vec<u8> {
        [&[0, 0, 0, 1, 0x46, 0x01, 0x50][..], &[0, 0, 0, 1], &TRAIL].concat()
    }

    #[test]
    fn keyframes_are_random_access_points() {
        assert!(is_keyframe(&keyframe()));
        assert!(!is_keyframe(&inter_frame()));
    }

    #[test]
    fn configuration_copies_the_profile_and_lists_the_parameter_sets() {
        let record = configuration(&keyframe()).unwrap();

        assert_eq!(&record[4..8], b"hvcC");
        let body = &record[8..];
        assert_eq!(body[0], 1);
        // Profile space/tier/profile, compatibility flags, constraint flags, level 4.1 (123).
        assert_eq!(
            body[1..13],
            [0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x7B]
        );
        assert_eq!(body[13..22], [0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0, 0x0F]);
        assert_eq!(body[22], 3);
        assert_eq!(body[23..28], [0xA0, 0, 1, 0, 6]);
        assert_eq!(&body[28..34], &VPS);
        assert_eq!(body[34..39], [0xA1, 0, 1, 0, 18]);
        assert_eq!(&body[39..57], &SPS);
        assert_eq!(body[57..62], [0xA2, 0, 1, 0, 4]);
        assert_eq!(&body[62..], &PPS);
        assert!(configuration(&inter_frame()).is_none());
    }

    #[test]
    fn samples_keep_only_picture_data() {
        assert_eq!(sample_units(&keyframe()), [&IDR[..]]);
        assert_eq!(sample_units(&inter_frame()), [&TRAIL[..]]);
    }
}
