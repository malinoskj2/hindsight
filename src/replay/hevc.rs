//! HEVC in MP4 (ISO/IEC 14496-15): length-prefixed NAL units per sample, parameter sets in
//! `hvcC`.

use super::{
    annexb::{nal_units, unescape},
    mp4::{self, NAL_LENGTH_BYTES},
};
use mp4_atom::{HvcCArray, Hvcc};
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
/// VPS id, sub-layer count and temporal id nesting flag.
const SPS_LAYERS: usize = 2;
/// `profile_tier_level`: profile space/tier/profile, compatibility flags, constraint flags, level.
const SPS_PROFILE: usize = 3;
const SPS_COMPATIBILITY_FLAGS: Range<usize> = 4..8;
const SPS_CONSTRAINT_FLAGS: Range<usize> = 8..14;
const SPS_LEVEL: usize = 14;
const CHROMA_420: u8 = 1;

pub(super) fn is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|unit| IRAP.contains(&nal_type(unit)))
}

/// The `hvcC` record, when the access unit carries a VPS, SPS and PPS.
pub(super) fn configuration(access_unit: &[u8]) -> Option<Vec<u8>> {
    let units: Vec<&[u8]> = nal_units(access_unit).collect();
    let of_type = |kind| units.iter().copied().find(|unit| nal_type(unit) == kind);
    let (vps, sps, pps) = (of_type(NAL_VPS)?, of_type(NAL_SPS)?, of_type(NAL_PPS)?);
    let mut record = record_fields(sps)?;
    record.arrays = [vps, sps, pps]
        .into_iter()
        .map(|unit| HvcCArray {
            completeness: true,
            nal_unit_type: nal_type(unit),
            nalus: vec![unit.to_vec()],
        })
        .collect();
    mp4::record(&record)
}

/// The `hvcC` fields copied from the SPS; the rest stay unknown or zero.
fn record_fields(sps: &[u8]) -> Option<Hvcc> {
    let header = unescape(sps, SPS_PROFILE_BYTES);
    if header.len() < SPS_PROFILE_BYTES {
        return None;
    }
    Some(Hvcc {
        general_profile_space: header[SPS_PROFILE] >> 6,
        general_tier_flag: header[SPS_PROFILE] & 0x20 != 0,
        general_profile_idc: header[SPS_PROFILE] & 0x1F,
        general_profile_compatibility_flags: header[SPS_COMPATIBILITY_FLAGS].try_into().ok()?,
        general_constraint_indicator_flags: header[SPS_CONSTRAINT_FLAGS].try_into().ok()?,
        general_level_idc: header[SPS_LEVEL],
        // 8-bit luma and chroma: the bit depths minus 8 stay 0.
        chroma_format_idc: CHROMA_420,
        num_temporal_layers: ((header[SPS_LAYERS] >> 1) & 0b111) + 1,
        temporal_id_nested: header[SPS_LAYERS] & 1 != 0,
        length_size_minus_one: NAL_LENGTH_BYTES - 1,
        ..Hvcc::new()
    })
}

/// The sample's NAL units without parameter sets (those live in `hvcC`) or delimiters.
pub(super) fn sample_units(access_unit: &[u8]) -> Vec<&[u8]> {
    nal_units(access_unit)
        .filter(|unit| unit.len() >= NAL_HEADER_BYTES)
        .filter(|unit| !matches!(nal_type(unit), NAL_VPS | NAL_SPS | NAL_PPS | NAL_AUD))
        .collect()
}

pub(super) fn nal_type(unit: &[u8]) -> u8 {
    (unit[0] >> 1) & NAL_TYPE_MASK
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::replay) const VPS: [u8; 6] = [0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF];
    pub(in crate::replay) const SPS: [u8; 18] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x7B,
    ];
    pub(in crate::replay) const PPS: [u8; 4] = [0x44, 0x01, 0xC1, 0x72];
    pub(in crate::replay) const IDR: [u8; 4] = [0x26, 0x01, 0xAF, 0x09];
    pub(in crate::replay) const TRAIL: [u8; 4] = [0x02, 0x01, 0xD0, 0x11];

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
