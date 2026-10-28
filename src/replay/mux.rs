//! What the MP4 and Matroska writers share.

#[cfg(test)]
use super::Packet;
use super::{StoreError, configuration, sample_parts};
use crate::config::Codec;
use mp4_atom::{Av1c, Avcc, Hvcc};
use std::io;
use thiserror::Error;

/// Length prefix on each NAL unit in a sample, as `avcC` and `hvcC` declare it.
pub(super) const NAL_LENGTH_BYTES: u8 = 4;
pub(super) const BOX_HEADER_BYTES: usize = 8;

pub(super) struct Sample {
    pub(super) index: u64,
    /// Bytes in the media data, length prefixes included.
    pub(super) size: u32,
    /// Frames until the next sample.
    pub(super) frames: u32,
    pub(super) keyframe: bool,
}

/// The decoder configuration record a video sample entry carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecoderConfig {
    Avc(Avcc),
    Hevc(Hvcc),
    Av1(Av1c),
}

/// The reason a clip could not be written. The message names only what failed, and the cause is
/// its `source()`.
#[derive(Debug, Error)]
pub(crate) enum MuxError {
    #[error("the replay buffer is empty")]
    Empty,
    #[error("the buffered video has no sequence header")]
    NoSequenceHeader,
    #[error("the clip is too large to index")]
    TooLarge,
    #[error("the buffered video changed while it was being saved")]
    Changed,
    #[error("build the clip's index")]
    Index(#[from] mp4_atom::Error),
    #[error("write the clip")]
    Io(#[from] io::Error),
    #[error("read the buffer back")]
    Read(#[from] StoreError),
}

pub(crate) type Visit<'a> = dyn FnMut(u64, bool, &[u8]) -> Result<(), MuxError> + 'a;

/// A clip's packets, oldest first starting from a keyframe, visited twice: once for the index,
/// then for the media.
pub(crate) trait Packets {
    fn visit(&self, each: &mut Visit<'_>) -> Result<(), MuxError>;
}

#[cfg(test)]
impl Packets for [std::sync::Arc<Packet>] {
    fn visit(&self, each: &mut Visit<'_>) -> Result<(), MuxError> {
        self.iter()
            .try_for_each(|packet| each(packet.index, packet.keyframe, &packet.data))
    }
}

/// The first pass. Returns the decoder record from the first packet carrying one, and a sample
/// per packet.
pub(super) fn index<P: Packets + ?Sized>(
    codec: Codec,
    packets: &P,
) -> Result<(DecoderConfig, Vec<Sample>), MuxError> {
    let mut seen = false;
    let mut decoder = None;
    let mut samples: Vec<Sample> = Vec::new();
    packets.visit(&mut |index, keyframe, data| {
        seen = true;
        if decoder.is_none() {
            decoder = configuration(codec, data);
        }
        add_sample(&mut samples, codec, index, keyframe, data)
    })?;
    if !seen {
        return Err(MuxError::Empty);
    }
    let decoder = decoder.ok_or(MuxError::NoSequenceHeader)?;
    if samples.is_empty() {
        return Err(MuxError::Empty);
    }
    Ok((decoder, samples))
}

fn add_sample(
    samples: &mut Vec<Sample>,
    codec: Codec,
    index: u64,
    keyframe: bool,
    data: &[u8],
) -> Result<(), MuxError> {
    let parts = sample_parts(codec, data);
    if parts.is_empty() {
        return Ok(());
    }
    let size = stored_size(&parts, prefix_bytes(codec))?;
    if let Some(last) = samples.last_mut() {
        last.frames = fit(index.saturating_sub(last.index).max(1))?;
    }
    samples.push(Sample {
        index,
        size,
        frames: 1,
        keyframe,
    });
    Ok(())
}

pub(super) fn indexed<'a>(
    samples: &'a [Sample],
    position: usize,
    codec: Codec,
    parts: &[&[u8]],
) -> Result<&'a Sample, MuxError> {
    let sample = samples.get(position).ok_or(MuxError::Changed)?;
    if sample.size != stored_size(parts, prefix_bytes(codec))? {
        return Err(MuxError::Changed);
    }
    Ok(sample)
}

pub(super) fn write_parts(
    output: &mut impl io::Write,
    codec: Codec,
    parts: &[&[u8]],
) -> Result<(), MuxError> {
    for part in parts {
        if length_prefixed(codec) {
            let length: u32 = fit(part.len())?;
            output.write_all(&length.to_be_bytes())?;
        }
        output.write_all(part)?;
    }
    Ok(())
}

fn prefix_bytes(codec: Codec) -> usize {
    if length_prefixed(codec) {
        usize::from(NAL_LENGTH_BYTES)
    } else {
        0
    }
}

fn stored_size(parts: &[&[u8]], prefix: usize) -> Result<u32, MuxError> {
    parts
        .iter()
        .map(|part| fit::<u32, _>(part.len() + prefix))
        .sum()
}

/// H.264 and HEVC samples are length-prefixed NAL units; AV1 OBUs carry their own sizes.
pub(super) const fn length_prefixed(codec: Codec) -> bool {
    !matches!(codec, Codec::Av1)
}

pub(super) fn fit<T: TryFrom<U>, U>(value: U) -> Result<T, MuxError> {
    T::try_from(value).map_err(|_| MuxError::TooLarge)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::replay::{Matrix, Stream, av1, h264, hevc};
    use std::sync::Arc;

    pub(in crate::replay) fn stream(codec: Codec) -> Stream {
        Stream {
            codec,
            width: 1920,
            height: 1080,
            frame_rate: 60,
            matrix: Matrix::Bt709,
        }
    }

    pub(in crate::replay) fn packet(codec: Codec, index: u64, keyframe: bool) -> Arc<Packet> {
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

    #[test]
    fn a_skipped_frame_lengthens_the_sample_before_it() {
        let packets = [
            packet(Codec::H264, 10, true),
            packet(Codec::H264, 11, false),
            packet(Codec::H264, 14, false),
        ];
        let (_, samples) = index(Codec::H264, &packets[..]).unwrap();
        let frames: Vec<u32> = samples.iter().map(|sample| sample.frames).collect();
        assert_eq!(frames, [1, 3, 1]);
        assert_eq!(samples[0].index, 10);
        assert!(samples[0].keyframe && !samples[1].keyframe);
    }

    #[test]
    fn a_sample_is_checked_against_its_index_on_the_second_pass() {
        let packets = [packet(Codec::H264, 0, true), packet(Codec::H264, 1, false)];
        let (_, samples) = index(Codec::H264, &packets[..]).unwrap();
        let parts = crate::replay::sample_parts(Codec::H264, &packets[1].data);
        assert_eq!(indexed(&samples, 1, Codec::H264, &parts).unwrap().index, 1);
        for position in [0, 2] {
            let changed = indexed(&samples, position, Codec::H264, &parts).map(|_| ());
            assert!(matches!(changed, Err(MuxError::Changed)), "{changed:?}");
        }
    }

    #[test]
    fn parts_are_length_prefixed_except_for_av1() {
        let parts: [&[u8]; 2] = [&[1, 2, 3], &[4]];
        let mut prefixed = Vec::new();
        write_parts(&mut prefixed, Codec::H264, &parts).unwrap();
        assert_eq!(prefixed, [0, 0, 0, 3, 1, 2, 3, 0, 0, 0, 1, 4]);
        let mut bare = Vec::new();
        write_parts(&mut bare, Codec::Av1, &parts).unwrap();
        assert_eq!(bare, [1, 2, 3, 4]);
        assert_eq!(stored_size(&parts, prefix_bytes(Codec::Hevc)).unwrap(), 12);
        assert_eq!(stored_size(&parts, prefix_bytes(Codec::Av1)).unwrap(), 4);
    }
}
