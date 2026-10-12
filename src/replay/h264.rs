//! H.264 in MP4 (ISO/IEC 14496-15): length-prefixed NAL units in each sample, with the
//! sequence and picture parameter sets in an `avcC` record.

use super::{
    annexb::nal_units,
    mp4::{self, NAL_LENGTH_BYTES},
};
use mp4_atom::{Avcc, AvccExt};

const NAL_TYPE_MASK: u8 = 0x1F;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
pub(super) const NAL_AUD: u8 = 9;
/// The profiles whose `avcC` record goes on to state the chroma format and bit depths: High,
/// High 10, High 4:2:2 and High 4:4:4 Predictive.
const HIGH_PROFILES: [u8; 4] = [100, 110, 122, 144];
/// 4:2:0, the `chroma_format_idc` the encoder is configured for.
const CHROMA_420: u8 = 1;

/// Whether the access unit holds an IDR picture.
pub(super) fn is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|unit| nal_type(unit) == NAL_IDR)
}

/// The `avcC` record for the parameter sets in `access_unit`, if it carries both: the profile,
/// compatibility flags and level from the SPS, then the one SPS and the one PPS.
pub(super) fn configuration(access_unit: &[u8]) -> Option<Vec<u8>> {
    let units: Vec<&[u8]> = nal_units(access_unit).collect();
    let of_type = |kind| units.iter().copied().find(|unit| nal_type(unit) == kind);
    let mut record = Avcc::new(of_type(NAL_SPS)?, of_type(NAL_PPS)?).ok()?;
    record.length_size = NAL_LENGTH_BYTES;
    // 8-bit luma and chroma, with no SPS extensions.
    if HIGH_PROFILES.contains(&record.avc_profile_indication) {
        record.ext = Some(AvccExt {
            chroma_format: CHROMA_420,
            ..AvccExt::default()
        });
    }
    mp4::record(&record)
}

/// The NAL units a stored sample keeps: parameter sets travel in the sample description and
/// access unit delimiters carry nothing an MP4 reader needs.
pub(super) fn sample_units(access_unit: &[u8]) -> Vec<&[u8]> {
    nal_units(access_unit)
        .filter(|unit| !matches!(nal_type(unit), NAL_SPS | NAL_PPS | NAL_AUD))
        .collect()
}

/// The `nal_unit_type` in a NAL unit's one-byte header.
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

    /// A keyframe access unit as NVENC writes it: delimiter, parameter sets, then the picture.
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

    /// An access unit holding one slice of a picture that refers to earlier ones.
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
        let record = configuration(&keyframe()).unwrap();

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
