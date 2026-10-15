//! H.264 in MP4 (ISO/IEC 14496-15): length-prefixed NAL units per sample, parameter sets in
//! `avcC`.

use super::{
    annexb::{find_units, nal_units, units_except},
    mp4::NAL_LENGTH_BYTES,
};
use mp4_atom::{Avcc, AvccExt};

const NAL_TYPE_MASK: u8 = 0x1F;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
pub(super) const NAL_AUD: u8 = 9;
/// Parameter sets are stored in `avcC` and delimiters are dropped. The remaining units of an
/// access unit form its sample.
const NOT_IN_SAMPLES: [u8; 3] = [NAL_SPS, NAL_PPS, NAL_AUD];
/// Profiles whose `avcC` record also carries the chroma format and bit depths.
const HIGH_PROFILES: [u8; 4] = [100, 110, 122, 144];
const CHROMA_420: u8 = 1;

pub(super) fn is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|unit| nal_type(unit) == NAL_IDR)
}

/// Returns the `avcC` record, when the access unit carries both an SPS and a PPS.
pub(super) fn configuration(access_unit: &[u8]) -> Option<Avcc> {
    let [sps, pps] = find_units(access_unit, nal_type, [NAL_SPS, NAL_PPS])?;
    let mut record = Avcc::new(sps, pps).ok()?;
    record.length_size = NAL_LENGTH_BYTES;
    // 8-bit luma and chroma, with no SPS extensions.
    if HIGH_PROFILES.contains(&record.avc_profile_indication) {
        record.ext = Some(AvccExt {
            chroma_format: CHROMA_420,
            ..AvccExt::default()
        });
    }
    Some(record)
}

pub(super) fn sample_units(access_unit: &[u8]) -> Vec<&[u8]> {
    units_except(access_unit, nal_type, &NOT_IN_SAMPLES).collect()
}

pub(super) fn nal_type(unit: &[u8]) -> u8 {
    unit[0] & NAL_TYPE_MASK
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::replay) const SPS: [u8; 6] = [0x67, 0x64, 0x00, 0x2A, 0xAC, 0x2B];
    pub(in crate::replay) const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];
    pub(in crate::replay) const IDR: [u8; 5] = [0x65, 0x88, 0x84, 0x00, 0x33];
    pub(in crate::replay) const SLICE: [u8; 4] = [0x41, 0x9A, 0x02, 0x10];

    pub(in crate::replay) fn keyframe() -> Vec<u8> {
        [
            &[0, 0, 0, 1, 0x09, 0x10][..],
            &[0, 0, 0, 1],
            &SPS,
            &[0, 0, 0, 1],
            &PPS,
            &[0, 0, 1],
            &IDR,
        ]
        .concat()
    }

    pub(in crate::replay) fn inter_frame() -> Vec<u8> {
        [&[0, 0, 0, 1][..], &SLICE].concat()
    }

    #[test]
    fn keyframes_carry_an_idr_picture() {
        assert!(is_keyframe(&keyframe()));
        assert!(!is_keyframe(&inter_frame()));
    }

    #[test]
    fn configuration_holds_the_parameter_sets() {
        let record = crate::replay::mp4::tests::record(&configuration(&keyframe()).unwrap());

        assert_eq!(&record[4..8], b"avcC");
        let body = &record[8..];
        assert_eq!(body[..6], [1, 0x64, 0x00, 0x2A, 0xFF, 0xE1]);
        assert_eq!(&body[8..14], &SPS);
        assert_eq!(&body[17..21], &PPS);
        assert_eq!(&body[21..], &[0xFD, 0xF8, 0xF8, 0]);
        assert!(configuration(&inter_frame()).is_none());
    }

    #[test]
    fn samples_keep_only_picture_data() {
        assert_eq!(sample_units(&keyframe()), [&IDR[..]]);
        assert_eq!(sample_units(&inter_frame()), [&SLICE[..]]);
    }
}
