//! AV1 in MP4: each sample is a temporal unit without its temporal delimiter, and `av1C`
//! repeats the sequence header.

use super::mp4;
use bitstream_io::{BigEndian, BitRead, BitReader};
use mp4_atom::Av1c;
use std::io;

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
const OBU_PADDING: u8 = 15;
const OBU_TYPE_SHIFT: u8 = 3;
const OBU_TYPE_MASK: u8 = 0x0F;
const OBU_EXTENSION_FLAG: u8 = 0x04;
const OBU_HAS_SIZE_FIELD: u8 = 0x02;
const MAX_LEB128_BYTES: usize = 8;
const KEY_FRAME: u32 = 0;
const PROFILE_MAIN: u32 = 0;
const PROFILE_HIGH: u32 = 1;
const PROFILE_PROFESSIONAL: u32 = 2;
/// Levels above 3.3 signal a tier.
const MAX_LEVEL_WITHOUT_TIER: u32 = 7;
/// sRGB, which the sequence header codes without chroma subsampling.
const SRGB: (u32, u32, u32) = (1, 13, 0);
const UNSPECIFIED_COLOUR: (u32, u32, u32) = (2, 2, 2);

type Bits<'a> = BitReader<&'a [u8], BigEndian>;

pub(super) fn is_keyframe(temporal_unit: &[u8]) -> bool {
    obus(temporal_unit)
        .find(|obu| matches!(obu.kind, OBU_FRAME_HEADER | OBU_FRAME))
        .and_then(|obu| {
            let mut bits = Bits::new(obu.payload);
            let show_existing_frame = bits.read_bit().ok()?;
            let frame_type = bits.read::<2, u32>().ok()?;
            Some(!show_existing_frame && frame_type == KEY_FRAME)
        })
        .unwrap_or(false)
}

pub(super) fn configuration(temporal_unit: &[u8]) -> Option<Vec<u8>> {
    let obu = obus(temporal_unit).find(|obu| obu.kind == OBU_SEQUENCE_HEADER)?;
    let sequence = SequenceHeader::parse(obu.payload).ok()?;
    let mut record = sequence.record()?;
    write_sized_obu(&mut record.config_obus, &obu);
    mp4::record(&record)
}

fn write_sized_obu(output: &mut Vec<u8>, obu: &Obu<'_>) {
    output.push(obu.header[0] | OBU_HAS_SIZE_FIELD);
    output.extend_from_slice(&obu.header[1..]);
    write_leb128(output, obu.payload.len());
    output.extend_from_slice(obu.payload);
}

pub(super) fn sample_obus(temporal_unit: &[u8]) -> Vec<&[u8]> {
    obus(temporal_unit)
        .filter(|obu| !matches!(obu.kind, OBU_TEMPORAL_DELIMITER | OBU_PADDING))
        .map(|obu| obu.bytes)
        .collect()
}

#[cfg(test)]
pub(super) fn temporal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut offset = 0;
    let mut starts = Vec::new();
    for obu in obus(stream) {
        if obu.kind == OBU_TEMPORAL_DELIMITER {
            starts.push(offset);
        }
        offset += obu.bytes.len();
    }
    starts.push(stream.len());
    starts
        .windows(2)
        .map(|pair| &stream[pair[0]..pair[1]])
        .collect()
}

struct Obu<'a> {
    kind: u8,
    header: &'a [u8],
    payload: &'a [u8],
    bytes: &'a [u8],
}

fn obus(mut data: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    std::iter::from_fn(move || {
        let obu = first_obu(data)?;
        data = &data[obu.bytes.len()..];
        Some(obu)
    })
}

fn first_obu(data: &[u8]) -> Option<Obu<'_>> {
    let &first = data.first()?;
    let header_length = if first & OBU_EXTENSION_FLAG == 0 {
        1
    } else {
        2
    };
    let after_header = data.get(header_length..)?;
    let (payload_start, payload_length) = if first & OBU_HAS_SIZE_FIELD == 0 {
        (header_length, after_header.len())
    } else {
        let (size, length) = leb128(after_header)?;
        (header_length + length, size)
    };
    let end = payload_start.checked_add(payload_length)?;
    Some(Obu {
        kind: obu_type(first),
        header: &data[..header_length],
        payload: data.get(payload_start..end)?,
        bytes: &data[..end],
    })
}

const fn obu_type(header: u8) -> u8 {
    (header >> OBU_TYPE_SHIFT) & OBU_TYPE_MASK
}

fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    for (index, byte) in data.iter().take(MAX_LEB128_BYTES).enumerate() {
        value |= u64::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, index + 1));
        }
    }
    None
}

fn write_leb128(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = u8::try_from(value & 0x7F).expect("a 7-bit value fits a byte");
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SequenceHeader {
    profile: u32,
    level: u32,
    tier: bool,
    colour: ColourConfig,
}

impl SequenceHeader {
    /// `sequence_header_obu()` (spec 5.5) through `color_config()`.
    fn parse(payload: &[u8]) -> io::Result<Self> {
        let mut bits = Bits::new(payload);
        let profile = bits.read::<3, u32>()?;
        let _still_picture = bits.read_bit()?;
        let reduced_still_picture_header = bits.read_bit()?;

        let (level, tier) = if reduced_still_picture_header {
            (bits.read::<5, u32>()?, false)
        } else {
            Self::first_operating_point(&mut bits)?
        };
        Self::skip_frame_size_and_tools(&mut bits, reduced_still_picture_header)?;
        let colour = ColourConfig::parse(&mut bits, profile)?;
        Ok(Self {
            profile,
            level,
            tier,
            colour,
        })
    }

    fn record(&self) -> Option<Av1c> {
        let colour = &self.colour;
        Some(Av1c {
            seq_profile: u8::try_from(self.profile).ok()?,
            seq_level_idx_0: u8::try_from(self.level).ok()?,
            seq_tier_0: self.tier,
            high_bitdepth: colour.high_bitdepth,
            twelve_bit: colour.twelve_bit,
            monochrome: colour.monochrome,
            chroma_subsampling_x: colour.subsampling_x,
            chroma_subsampling_y: colour.subsampling_y,
            chroma_sample_position: u8::try_from(colour.chroma_sample_position).ok()?,
            initial_presentation_delay: None,
            config_obus: Vec::new(),
        })
    }

    fn first_operating_point(bits: &mut Bits<'_>) -> io::Result<(u32, bool)> {
        let timing_info_present = bits.read_bit()?;
        if timing_info_present {
            Self::skip_timing_info(bits)?;
        }
        let buffer_delay_length = if timing_info_present && bits.read_bit()? {
            Some(Self::decoder_model_info(bits)?)
        } else {
            None
        };
        let initial_display_delay_present = bits.read_bit()?;
        let operating_points = bits.read::<5, u32>()? + 1;
        let mut first = None;
        for _ in 0..operating_points {
            let _idc = bits.read::<12, u32>()?;
            let point_level = bits.read::<5, u32>()?;
            let point_tier = point_level > MAX_LEVEL_WITHOUT_TIER && bits.read_bit()?;
            if let Some(buffer_delay_length) = buffer_delay_length
                && bits.read_bit()?
            {
                bits.skip(2 * buffer_delay_length + 1)?;
            }
            if initial_display_delay_present && bits.read_bit()? {
                bits.skip(4)?;
            }
            first.get_or_insert((point_level, point_tier));
        }
        first.ok_or_else(|| io::ErrorKind::InvalidData.into())
    }

    fn skip_timing_info(bits: &mut Bits<'_>) -> io::Result<()> {
        bits.skip(64)?;
        if bits.read_bit()? {
            uvlc(bits)?;
        }
        Ok(())
    }

    fn decoder_model_info(bits: &mut Bits<'_>) -> io::Result<u32> {
        let buffer_delay_length = bits.read::<5, u32>()? + 1;
        bits.skip(32 + 5 + 5)?;
        Ok(buffer_delay_length)
    }

    fn skip_frame_size_and_tools(
        bits: &mut Bits<'_>,
        reduced_still_picture_header: bool,
    ) -> io::Result<()> {
        let width_bits = bits.read::<4, u32>()? + 1;
        let height_bits = bits.read::<4, u32>()? + 1;
        bits.skip(width_bits + height_bits)?;
        if !reduced_still_picture_header && bits.read_bit()? {
            // Frame id lengths.
            bits.skip(4 + 3)?;
        }
        // 128×128 superblocks, filter intra, intra edge filter.
        bits.skip(3)?;
        if !reduced_still_picture_header {
            Self::skip_inter_tools(bits)?;
        }
        // Superres, CDEF, loop restoration.
        bits.skip(3)
    }

    fn skip_inter_tools(bits: &mut Bits<'_>) -> io::Result<()> {
        // Inter-intra, masked compound, warped motion, dual filter.
        bits.skip(4)?;
        let order_hint = bits.read_bit()?;
        if order_hint {
            // Joint compound, reference frame motion vectors.
            bits.skip(2)?;
        }
        let force_screen_content_tools = if bits.read_bit()? {
            2
        } else {
            bits.read::<1, u32>()?
        };
        if force_screen_content_tools > 0 && !bits.read_bit()? {
            bits.skip(1)?;
        }
        if order_hint {
            bits.skip(3)?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools)]
struct ColourConfig {
    high_bitdepth: bool,
    twelve_bit: bool,
    monochrome: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    chroma_sample_position: u32,
}

impl ColourConfig {
    /// `color_config()` (spec 5.5.2).
    fn parse(bits: &mut Bits<'_>, profile: u32) -> io::Result<Self> {
        let high_bitdepth = bits.read_bit()?;
        let twelve_bit = profile == PROFILE_PROFESSIONAL && high_bitdepth && bits.read_bit()?;
        let monochrome = profile != PROFILE_HIGH && bits.read_bit()?;
        let colours = if bits.read_bit()? {
            (
                bits.read::<8, u32>()?,
                bits.read::<8, u32>()?,
                bits.read::<8, u32>()?,
            )
        } else {
            UNSPECIFIED_COLOUR
        };
        let (subsampling_x, subsampling_y, chroma_sample_position) = if monochrome {
            (true, true, 0)
        } else if colours == SRGB {
            (false, false, 0)
        } else {
            Self::chroma(bits, profile, twelve_bit)?
        };
        Ok(Self {
            high_bitdepth,
            twelve_bit,
            monochrome,
            subsampling_x,
            subsampling_y,
            chroma_sample_position,
        })
    }

    fn chroma(
        bits: &mut Bits<'_>,
        profile: u32,
        twelve_bit: bool,
    ) -> io::Result<(bool, bool, u32)> {
        let _color_range = bits.read_bit()?;
        let (subsampling_x, subsampling_y) = match profile {
            PROFILE_MAIN => (true, true),
            PROFILE_HIGH => (false, false),
            _ if twelve_bit => {
                let x = bits.read_bit()?;
                (x, x && bits.read_bit()?)
            }
            _ => (true, false),
        };
        let chroma_sample_position = if subsampling_x && subsampling_y {
            bits.read::<2, u32>()?
        } else {
            0
        };
        Ok((subsampling_x, subsampling_y, chroma_sample_position))
    }
}

/// `uvlc()`: leading zeros, a one, then as many value bits.
fn uvlc(bits: &mut Bits<'_>) -> io::Result<u32> {
    let mut zeros = 0;
    while !bits.read_bit()? {
        zeros += 1;
        if zeros >= 32 {
            return Ok(u32::MAX);
        }
    }
    Ok(bits.read_var::<u32>(zeros)? + ((1 << zeros) - 1))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use bitstream_io::{BitWrite, BitWriter};

    struct Writer(BitWriter<Vec<u8>, BigEndian>);

    impl Writer {
        fn new() -> Self {
            Self(BitWriter::new(Vec::new()))
        }

        fn put(&mut self, count: u32, value: u32) -> &mut Self {
            self.0.write_var(count, value).unwrap();
            self
        }

        fn into_bytes(mut self) -> Vec<u8> {
            self.0.byte_align().unwrap();
            self.0.into_writer()
        }
    }

    fn elaborate_sequence_header() -> Vec<u8> {
        let mut bits = Writer::new();
        bits.put(3, 0).put(1, 0).put(1, 0);
        bits.put(1, 1).put(32, 1).put(32, 60).put(1, 1).put(1, 1);
        bits.put(1, 1).put(5, 9).put(32, 1).put(5, 4).put(5, 4);
        bits.put(1, 1);
        bits.put(5, 1);
        bits.put(12, 0x103)
            .put(5, 8)
            .put(1, 1)
            .put(1, 1)
            .put(10, 5)
            .put(10, 7)
            .put(1, 0);
        bits.put(1, 1).put(4, 3);
        bits.put(12, 0x101).put(5, 4).put(1, 0).put(1, 0);
        bits.put(4, 10).put(4, 10).put(11, 1919).put(11, 1079);
        bits.put(1, 1).put(4, 5).put(3, 2);
        bits.put(3, 0b011);
        bits.put(4, 0b0011).put(1, 1).put(2, 0b11);
        bits.put(1, 0).put(1, 1).put(1, 0).put(1, 0);
        bits.put(3, 6);
        bits.put(3, 0b011);
        bits.put(1, 0)
            .put(1, 0)
            .put(1, 1)
            .put(8, 1)
            .put(8, 1)
            .put(8, 1)
            .put(1, 0)
            .put(2, 1);
        bits.put(1, 0);
        bits.into_bytes()
    }

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![(kind << OBU_TYPE_SHIFT) | OBU_HAS_SIZE_FIELD];
        write_leb128(&mut bytes, payload.len());
        bytes.extend_from_slice(payload);
        bytes
    }

    pub(in crate::replay) fn keyframe() -> Vec<u8> {
        [
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_SEQUENCE_HEADER, &elaborate_sequence_header()),
            obu(OBU_FRAME, &[0b0001_0000, 0xAB, 0xCD]),
        ]
        .concat()
    }

    pub(in crate::replay) fn inter_frame() -> Vec<u8> {
        [
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_FRAME, &[0b0011_0000, 0x12]),
        ]
        .concat()
    }

    #[test]
    fn keyframes_are_told_from_inter_frames() {
        assert!(is_keyframe(&keyframe()));
        assert!(!is_keyframe(&inter_frame()));
        assert!(!is_keyframe(&obu(OBU_FRAME_HEADER, &[0b1000_0000])));
    }

    #[test]
    fn configuration_repeats_the_sequence_header() {
        let record = configuration(&keyframe()).unwrap();

        assert_eq!(&record[4..8], b"av1C");
        let body = &record[8..];
        assert_eq!(body[..4], [0x81, 0x08, 0x8D, 0]);
        assert_eq!(
            body[4..],
            obu(OBU_SEQUENCE_HEADER, &elaborate_sequence_header())
        );
        assert!(configuration(&inter_frame()).is_none());
    }

    #[test]
    fn samples_drop_delimiters_and_padding() {
        let unit = [keyframe(), obu(OBU_PADDING, &[0; 3])].concat();
        let kept = sample_obus(&unit);

        assert_eq!(kept.len(), 2);
        assert_eq!(obu_type(kept[0][0]), OBU_SEQUENCE_HEADER);
        assert_eq!(kept[1], obu(OBU_FRAME, &[0b0001_0000, 0xAB, 0xCD]));
        assert_eq!(
            sample_obus(&inter_frame()),
            [&obu(OBU_FRAME, &[0b0011_0000, 0x12])[..]]
        );
    }

    #[test]
    fn reads_every_optional_part_of_a_sequence_header() {
        assert_eq!(
            SequenceHeader::parse(&elaborate_sequence_header()).unwrap(),
            SequenceHeader {
                profile: 0,
                level: 8,
                tier: true,
                colour: ColourConfig {
                    high_bitdepth: false,
                    twelve_bit: false,
                    monochrome: false,
                    subsampling_x: true,
                    subsampling_y: true,
                    chroma_sample_position: 1,
                },
            }
        );
        assert!(SequenceHeader::parse(&elaborate_sequence_header()[..20]).is_err());
    }

    #[test]
    fn leb128_round_trips() {
        for value in [0, 1, 127, 128, 300, 1 << 20] {
            let mut bytes = Vec::new();
            write_leb128(&mut bytes, value);
            assert_eq!(leb128(&bytes), Some((value, bytes.len())));
        }
        assert_eq!(leb128(&[0x80, 0x80]), None);
    }
}
