//! A minimal MP4 writer for one video track in H.264, HEVC or AV1. The index (`moov`) is written
//! before the media data, so players can start a clip without reading to its end; with every
//! packet already in memory, the offsets are known up front.

use super::{Packet, Stream, configuration, sample_parts};
use crate::config::Codec;
use std::{io, sync::Arc};
use thiserror::Error;

/// Media ticks per frame; the track's time scale is the frame rate times this.
const TICKS_PER_FRAME: u32 = 1000;
/// The movie header's time scale: milliseconds.
const MOVIE_TIMESCALE: u32 = 1000;
const TRACK_ID: u32 = 1;
/// A 16-byte `mdat` header with a 64-bit size, so clips larger than 4 GB need no special case.
const MDAT_HEADER_BYTES: u64 = 16;
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
/// `und`, packed as three 5-bit letters.
const UNDETERMINED_LANGUAGE: u16 = 0x55C4;
/// BT.709 primaries, transfer and matrix, limited range: what the encoder signals.
const BT709: u16 = 1;

#[derive(Debug, Error)]
pub(crate) enum MuxError {
    #[error("the replay buffer is empty")]
    Empty,
    #[error("the buffered video has no sequence header")]
    NoSequenceHeader,
    #[error("the clip is too large for an MP4 index")]
    TooLarge,
    #[error("write the clip: {0}")]
    Io(#[from] io::Error),
}

struct Sample<'a> {
    parts: Vec<&'a [u8]>,
    size: u32,
    duration: u32,
    keyframe: bool,
}

/// Writes `packets`, oldest first and starting on a keyframe, as a complete MP4 file.
pub(crate) fn write(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
) -> Result<(), MuxError> {
    let decoder = packets
        .iter()
        .find_map(|packet| configuration(stream.codec, &packet.data))
        .ok_or(MuxError::NoSequenceHeader)?;
    let samples = samples(stream.codec, packets)?;
    if samples.is_empty() {
        return Err(MuxError::Empty);
    }

    let file_type = file_type(stream.codec);
    // Offsets are 64-bit whatever their value, so the index has the same length either way.
    let index_length = movie(stream, &decoder, &samples, 0)?.len();
    let media_offset = (file_type.len() + index_length) as u64 + MDAT_HEADER_BYTES;
    let index = movie(stream, &decoder, &samples, media_offset)?;
    let prefixed = length_prefixed(stream.codec);
    let media_bytes: u64 = samples.iter().map(|sample| u64::from(sample.size)).sum();

    output.write_all(&file_type)?;
    output.write_all(&index)?;
    output.write_all(&1_u32.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(&(MDAT_HEADER_BYTES + media_bytes).to_be_bytes())?;
    for sample in &samples {
        for part in &sample.parts {
            if prefixed {
                output.write_all(&length(part.len())?.to_be_bytes())?;
            }
            output.write_all(part)?;
        }
    }
    output.flush()?;
    Ok(())
}

/// Each packet as a sample; a frame index that skips ahead lengthens the previous sample.
fn samples(codec: Codec, packets: &[Arc<Packet>]) -> Result<Vec<Sample<'_>>, MuxError> {
    let prefix = if length_prefixed(codec) { 4 } else { 0 };
    let mut samples: Vec<Sample<'_>> = Vec::with_capacity(packets.len());
    let mut previous_index = None;
    for packet in packets {
        let parts = sample_parts(codec, &packet.data);
        if parts.is_empty() {
            continue;
        }
        let size = parts
            .iter()
            .map(|part| length(part.len() + prefix))
            .sum::<Result<u32, _>>()?;
        if let (Some(last), Some(previous)) = (samples.last_mut(), previous_index) {
            let frames = u32::try_from(packet.index.saturating_sub(previous).max(1))
                .map_err(|_| MuxError::TooLarge)?;
            last.duration = frames
                .checked_mul(TICKS_PER_FRAME)
                .ok_or(MuxError::TooLarge)?;
        }
        previous_index = Some(packet.index);
        samples.push(Sample {
            parts,
            size,
            duration: TICKS_PER_FRAME,
            keyframe: packet.keyframe,
        });
    }
    Ok(samples)
}

fn length(bytes: usize) -> Result<u32, MuxError> {
    u32::try_from(bytes).map_err(|_| MuxError::TooLarge)
}

/// H.264 and HEVC samples are NAL units behind four-byte lengths; AV1 OBUs carry their own.
const fn length_prefixed(codec: Codec) -> bool {
    !matches!(codec, Codec::Av1)
}

fn file_type(codec: Codec) -> Vec<u8> {
    let codec_brand: &[u8] = match codec {
        Codec::H264 => b"avc1",
        Codec::Hevc => b"",
        Codec::Av1 => b"av01",
    };
    let body = [
        &b"isom"[..],
        &0x200_u32.to_be_bytes(),
        b"isom",
        b"iso2",
        codec_brand,
        b"mp41",
    ]
    .concat();
    atom(*b"ftyp", &body)
}

fn movie(
    stream: &Stream,
    decoder: &[u8],
    samples: &[Sample<'_>],
    media_offset: u64,
) -> Result<Vec<u8>, MuxError> {
    let timescale = stream
        .frame_rate
        .checked_mul(TICKS_PER_FRAME)
        .ok_or(MuxError::TooLarge)?;
    let media_duration: u64 = samples
        .iter()
        .map(|sample| u64::from(sample.duration))
        .sum();
    let movie_duration = media_duration * u64::from(MOVIE_TIMESCALE) / u64::from(timescale);
    let media_duration = u32::try_from(media_duration).map_err(|_| MuxError::TooLarge)?;
    let movie_duration = u32::try_from(movie_duration).map_err(|_| MuxError::TooLarge)?;

    let mut header = Vec::new();
    header.extend_from_slice(&[0; 8]);
    header.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    header.extend_from_slice(&movie_duration.to_be_bytes());
    header.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
    header.extend_from_slice(&0x0100_u16.to_be_bytes());
    header.extend_from_slice(&[0; 10]);
    header.extend_from_slice(&matrix());
    header.extend_from_slice(&[0; 24]);
    header.extend_from_slice(&(TRACK_ID + 1).to_be_bytes());

    let track = [
        track_header(stream, movie_duration),
        atom(
            *b"mdia",
            &[
                media_header(timescale, media_duration),
                handler(),
                atom(
                    *b"minf",
                    &[
                        full_atom(*b"vmhd", 0, 1, &[0; 8]),
                        data_information(),
                        sample_table(stream, decoder, samples, media_offset)?,
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    ]
    .concat();

    Ok(atom(
        *b"moov",
        &[full_atom(*b"mvhd", 0, 0, &header), atom(*b"trak", &track)].concat(),
    ))
}

fn track_header(stream: &Stream, duration: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&TRACK_ID.to_be_bytes());
    body.extend_from_slice(&[0; 4]);
    body.extend_from_slice(&duration.to_be_bytes());
    body.extend_from_slice(&[0; 16]);
    body.extend_from_slice(&matrix());
    body.extend_from_slice(&(stream.width << 16).to_be_bytes());
    body.extend_from_slice(&(stream.height << 16).to_be_bytes());
    // Enabled and part of the presentation.
    full_atom(*b"tkhd", 0, 0x3, &body)
}

fn media_header(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&duration.to_be_bytes());
    body.extend_from_slice(&UNDETERMINED_LANGUAGE.to_be_bytes());
    body.extend_from_slice(&[0; 2]);
    full_atom(*b"mdhd", 0, 0, &body)
}

fn handler() -> Vec<u8> {
    let body = [&[0; 4][..], b"vide", &[0; 12], b"VideoHandler\0"].concat();
    full_atom(*b"hdlr", 0, 0, &body)
}

fn data_information() -> Vec<u8> {
    // One entry: the media is in this file.
    let reference = full_atom(*b"url ", 0, 1, &[]);
    let table = full_atom(
        *b"dref",
        0,
        0,
        &[&1_u32.to_be_bytes()[..], &reference].concat(),
    );
    atom(*b"dinf", &table)
}

fn sample_table(
    stream: &Stream,
    decoder: &[u8],
    samples: &[Sample<'_>],
    media_offset: u64,
) -> Result<Vec<u8>, MuxError> {
    let count = length(samples.len())?;

    let mut times = Vec::new();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for sample in samples {
        match runs.last_mut() {
            Some((run, duration)) if *duration == sample.duration => *run += 1,
            _ => runs.push((1, sample.duration)),
        }
    }
    times.extend_from_slice(&length(runs.len())?.to_be_bytes());
    for (run, duration) in runs {
        times.extend_from_slice(&run.to_be_bytes());
        times.extend_from_slice(&duration.to_be_bytes());
    }

    let keyframes: Vec<u32> = (1..=count)
        .zip(samples)
        .filter(|(_, sample)| sample.keyframe)
        .map(|(number, _)| number)
        .collect();
    let mut sync = length(keyframes.len())?.to_be_bytes().to_vec();
    for number in keyframes {
        sync.extend_from_slice(&number.to_be_bytes());
    }

    // Every sample sits in one chunk.
    let chunks = [1_u32, 1, count, 1]
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect::<Vec<u8>>();

    let mut sizes = [0_u32, count]
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect::<Vec<u8>>();
    for sample in samples {
        sizes.extend_from_slice(&sample.size.to_be_bytes());
    }

    let offsets = [&1_u32.to_be_bytes()[..], &media_offset.to_be_bytes()].concat();

    Ok(atom(
        *b"stbl",
        &[
            full_atom(
                *b"stsd",
                0,
                0,
                &[&1_u32.to_be_bytes()[..], &visual_entry(stream, decoder)?].concat(),
            ),
            full_atom(*b"stts", 0, 0, &times),
            full_atom(*b"stss", 0, 0, &sync),
            full_atom(*b"stsc", 0, 0, &chunks),
            full_atom(*b"stsz", 0, 0, &sizes),
            full_atom(*b"co64", 0, 0, &offsets),
        ]
        .concat(),
    ))
}

/// The sample description: a visual sample entry with the decoder configuration record.
fn visual_entry(stream: &Stream, decoder: &[u8]) -> Result<Vec<u8>, MuxError> {
    let width = u16::try_from(stream.width).map_err(|_| MuxError::TooLarge)?;
    let height = u16::try_from(stream.height).map_err(|_| MuxError::TooLarge)?;
    // Parameter sets live only in the configuration record (`hvc1`, not `hev1`).
    let kind = match stream.codec {
        Codec::H264 => b"avc1",
        Codec::Hevc => b"hvc1",
        Codec::Av1 => b"av01",
    };

    let mut body = Vec::new();
    body.extend_from_slice(&[0; 6]);
    body.extend_from_slice(&1_u16.to_be_bytes());
    body.extend_from_slice(&[0; 16]);
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    // 72 dpi in both directions.
    body.extend_from_slice(&0x0048_0000_u32.to_be_bytes());
    body.extend_from_slice(&0x0048_0000_u32.to_be_bytes());
    body.extend_from_slice(&[0; 4]);
    body.extend_from_slice(&1_u16.to_be_bytes());
    body.extend_from_slice(&[0; 32]);
    body.extend_from_slice(&0x0018_u16.to_be_bytes());
    body.extend_from_slice(&(-1_i16).to_be_bytes());
    body.extend_from_slice(decoder);
    body.extend_from_slice(&colour());
    Ok(atom(*kind, &body))
}

fn colour() -> Vec<u8> {
    let mut body = b"nclx".to_vec();
    for value in [BT709; 3] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    // Limited range.
    body.push(0);
    atom(*b"colr", &body)
}

fn matrix() -> Vec<u8> {
    UNITY_MATRIX
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

pub(super) fn atom(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    // Index atoms stay far below 4 GB; only `mdat` can grow past it, and it is written apart.
    let size = u32::try_from(body.len() + 8).expect("an index atom fits a 32-bit size");
    [&size.to_be_bytes()[..], &kind, body].concat()
}

fn full_atom(kind: [u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let header = (u32::from(version) << 24) | (flags & 0x00FF_FFFF);
    atom(kind, &[&header.to_be_bytes()[..], body].concat())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::replay::{av1, h264, hevc};

    /// The atoms in `bytes`, as (kind, body) pairs.
    pub(in crate::replay) fn atoms(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut found = Vec::new();
        let mut rest = bytes;
        while rest.len() >= 8 {
            let mut size = u64::from(u32::from_be_bytes(rest[..4].try_into().unwrap()));
            let kind: [u8; 4] = rest[4..8].try_into().unwrap();
            let mut header = 8;
            if size == 1 {
                size = u64::from_be_bytes(rest[8..16].try_into().unwrap());
                header = 16;
            }
            let size = usize::try_from(size).unwrap();
            found.push((kind, &rest[header..size]));
            rest = &rest[size..];
        }
        assert!(rest.is_empty(), "trailing bytes after the last atom");
        found
    }

    fn child(body: &[u8], kind: [u8; 4]) -> &[u8] {
        atoms(body)
            .into_iter()
            .find(|(found, _)| *found == kind)
            .map_or_else(
                || panic!("no {} atom", String::from_utf8_lossy(&kind)),
                |(_, body)| body,
            )
    }

    fn sample_table(file: &[u8]) -> &[u8] {
        let movie = atoms(file)[1].1;
        child(
            child(child(child(movie, *b"trak"), *b"mdia"), *b"minf"),
            *b"stbl",
        )
    }

    fn words(body: &[u8]) -> Vec<u32> {
        body.as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_be_bytes(*word))
            .collect()
    }

    fn stream(codec: Codec) -> Stream {
        Stream {
            codec,
            width: 1920,
            height: 1080,
            frame_rate: 60,
        }
    }

    fn packet(codec: Codec, index: u64, keyframe: bool) -> Arc<Packet> {
        let data = match (codec, keyframe) {
            (Codec::H264, true) => h264::tests::keyframe(),
            (Codec::H264, false) => h264::tests::inter_frame(),
            (Codec::Hevc, true) => hevc::tests::keyframe(),
            (Codec::Hevc, false) => hevc::tests::inter_frame(),
            (Codec::Av1, true) => av1::tests::keyframe(),
            (Codec::Av1, false) => av1::tests::inter_frame(),
        };
        Arc::new(Packet {
            index,
            keyframe,
            data: data.into(),
        })
    }

    fn mux(codec: Codec, packets: &[Arc<Packet>]) -> Vec<u8> {
        let mut file = Vec::new();
        write(&mut file, &stream(codec), packets).unwrap();
        file
    }

    #[test]
    fn writes_index_before_media_with_matching_offsets() {
        let packets: Vec<_> = (0..6)
            .map(|index| packet(Codec::H264, index, index % 3 == 0))
            .collect();
        let file = mux(Codec::H264, &packets);

        let top = atoms(&file);
        let kinds: Vec<_> = top.iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [b"ftyp", b"moov", b"mdat"]);

        let table = sample_table(&file);
        assert_eq!(words(child(table, *b"stsz")), [0, 0, 6, 9, 8, 8, 9, 8, 8]);
        assert_eq!(words(child(table, *b"stss")), [0, 2, 1, 4]);
        assert_eq!(words(child(table, *b"stts")), [0, 1, 6, 1000]);
        assert_eq!(words(child(table, *b"stsc")), [0, 1, 1, 6, 1]);

        let offsets = child(table, *b"co64");
        let offset =
            usize::try_from(u64::from_be_bytes(offsets[8..16].try_into().unwrap())).unwrap();
        assert_eq!(&file[offset..offset + 4], &5_u32.to_be_bytes());
        assert_eq!(&file[offset + 4..offset + 9], &h264::tests::IDR);
        assert_eq!(&file[offset + 13..offset + 17], &h264::tests::SLICE);
        assert_eq!(top[2].1.len(), 9 * 2 + 8 * 4);
    }

    #[test]
    fn describes_the_stream_and_its_decoder_configuration() {
        let file = mux(Codec::H264, &[packet(Codec::H264, 0, true)]);
        let movie = atoms(&file)[1].1;

        let header = child(movie, *b"mvhd");
        assert_eq!(words(&header[12..20]), [1000, 16]);
        let track = child(movie, *b"trak");
        let track_header = child(track, *b"tkhd");
        assert_eq!(words(&track_header[76..84]), [1920 << 16, 1080 << 16]);
        let media = child(track, *b"mdia");
        assert_eq!(words(&child(media, *b"mdhd")[12..20]), [60_000, 1000]);

        let description = child(sample_table(&file), *b"stsd");
        let visual = child(&description[8..], *b"avc1");
        assert_eq!(&visual[24..28], &[0x07, 0x80, 0x04, 0x38]);
        let configuration = child(&visual[78..], *b"avcC");
        assert_eq!(&configuration[8..14], &h264::tests::SPS);
        assert_eq!(&configuration[17..21], &h264::tests::PPS);
        assert_eq!(child(&visual[78..], *b"colr"), b"nclx\0\x01\0\x01\0\x01\0");
    }

    #[test]
    fn each_codec_gets_its_sample_entry_and_framing() {
        for (codec, kind, record, brand, first_sample) in [
            (
                Codec::Hevc,
                b"hvc1",
                b"hvcC",
                None,
                hevc::tests::IDR.len() + 4,
            ),
            (Codec::Av1, b"av01", b"av1C", Some(b"av01"), 0),
        ] {
            let packets = [packet(codec, 0, true), packet(codec, 1, false)];
            let file = mux(codec, &packets);

            let file_type = atoms(&file)[0].1;
            assert_eq!(
                file_type.as_chunks::<4>().0.contains(b"av01"),
                brand.is_some()
            );
            let table = sample_table(&file);
            let visual = child(&child(table, *b"stsd")[8..], *kind);
            child(&visual[78..], *record);

            let sizes = words(child(table, *b"stsz"));
            if codec == Codec::Hevc {
                assert_eq!(sizes[3] as usize, first_sample);
            } else {
                // AV1 samples are the OBUs as they are, minus the temporal delimiter.
                let kept: usize = av1::sample_obus(&packets[0].data)
                    .iter()
                    .map(|obu| obu.len())
                    .sum();
                assert_eq!(sizes[3] as usize, kept);
                let offsets = child(table, *b"co64");
                let offset =
                    usize::try_from(u64::from_be_bytes(offsets[8..16].try_into().unwrap()))
                        .unwrap();
                assert_eq!(
                    file[offset] >> 3,
                    1,
                    "the sample starts with the sequence header"
                );
            }
        }
    }

    #[test]
    fn skipped_frames_lengthen_the_previous_sample() {
        let packets = [
            packet(Codec::H264, 10, true),
            packet(Codec::H264, 11, false),
            packet(Codec::H264, 14, false),
        ];
        let file = mux(Codec::H264, &packets);

        assert_eq!(
            words(child(sample_table(&file), *b"stts")),
            [0, 3, 1, 1000, 1, 3000, 1, 1000]
        );
        assert_eq!(
            words(&child(atoms(&file)[1].1, *b"mvhd")[12..20]),
            [1000, 83]
        );
    }

    #[test]
    fn refuses_streams_without_a_sequence_header() {
        let mut file = Vec::new();
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            assert!(matches!(
                write(&mut file, &stream(codec), &[packet(codec, 0, false)]),
                Err(MuxError::NoSequenceHeader)
            ));
        }
        assert!(matches!(
            write(&mut file, &stream(Codec::H264), &[]),
            Err(MuxError::NoSequenceHeader)
        ));
        assert_eq!(file.len(), 0);
    }
}
