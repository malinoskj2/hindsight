//! HEVC in MP4 (ISO/IEC 14496-15): length-prefixed NAL units per sample, parameter sets in
//! `hvcC`.

use super::{
    annexb::{find_units, nal_units, unescape, units_except},
    mp4::NAL_LENGTH_BYTES,
};
use bitstream_io::{BigEndian, BitRead, BitReader};
use mp4_atom::{HvcCArray, Hvcc};
use std::{io, ops::RangeInclusive};

const NAL_TYPE_MASK: u8 = 0x3F;
const NAL_HEADER_BYTES: usize = 2;
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
pub(super) const NAL_AUD: u8 = 35;
/// Intra random access points: BLA, IDR and CRA pictures.
const IRAP: RangeInclusive<u8> = 16..=21;
/// Parameter sets are stored in `hvcC` and delimiters are dropped. The remaining units of an
/// access unit form its sample.
const NOT_IN_SAMPLES: [u8; 4] = [NAL_VPS, NAL_SPS, NAL_PPS, NAL_AUD];
/// Bytes of an SPS up to and including `general_level_idc`, after emulation prevention.
const SPS_PROFILE_BYTES: usize = 15;
const CHROMA_420: u8 = 1;

pub(super) fn is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|unit| IRAP.contains(&nal_type(unit)))
}

/// Returns the `hvcC` record, when the access unit carries a VPS, SPS and PPS.
pub(super) fn configuration(access_unit: &[u8]) -> Option<Hvcc> {
    let parameter_sets = find_units(access_unit, nal_type, [NAL_VPS, NAL_SPS, NAL_PPS])?;
    let [_, sps, _] = parameter_sets;
    let mut record = record_fields(sps).ok()?;
    record.arrays = parameter_sets
        .into_iter()
        .map(|unit| HvcCArray {
            completeness: true,
            nal_unit_type: nal_type(unit),
            nalus: vec![unit.to_vec()],
        })
        .collect();
    Some(record)
}

/// Returns the `hvcC` fields copied from the SPS (7.3.2.2 through `profile_tier_level`). The
/// rest stay unknown or zero.
fn record_fields(sps: &[u8]) -> io::Result<Hvcc> {
    let header = unescape(sps, SPS_PROFILE_BYTES);
    let mut bits = BitReader::<_, BigEndian>::new(&header[..]);
    let _nal_header = bits.read::<16, u16>()?;
    let _video_parameter_set_id = bits.read::<4, u8>()?;
    let max_sub_layers_minus1 = bits.read::<3, u8>()?;
    let temporal_id_nested = bits.read_bit()?;
    let general_profile_space = bits.read::<2, u8>()?;
    let general_tier_flag = bits.read_bit()?;
    let general_profile_idc = bits.read::<5, u8>()?;
    let general_profile_compatibility_flags = bits.read::<32, u32>()?.to_be_bytes();
    let [_, _, general_constraint_indicator_flags @ ..] = bits.read::<48, u64>()?.to_be_bytes();
    let general_level_idc = bits.read::<8, u8>()?;
    Ok(Hvcc {
        general_profile_space,
        general_tier_flag,
        general_profile_idc,
        general_profile_compatibility_flags,
        general_constraint_indicator_flags,
        general_level_idc,
        // 8-bit luma and chroma: the bit depths minus 8 stay 0.
        chroma_format_idc: CHROMA_420,
        num_temporal_layers: max_sub_layers_minus1 + 1,
        temporal_id_nested,
        length_size_minus_one: NAL_LENGTH_BYTES - 1,
        ..Hvcc::new()
    })
}

pub(super) fn sample_units(access_unit: &[u8]) -> Vec<&[u8]> {
    units_except(access_unit, nal_type, &NOT_IN_SAMPLES)
        .filter(|unit| unit.len() >= NAL_HEADER_BYTES)
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
        let record = crate::replay::mp4::tests::record(&configuration(&keyframe()).unwrap());

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
