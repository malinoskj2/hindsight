//! Reading a clip back: where its video samples and AAC frames are in the file and when they
//! play, and the video samples in the form a decoder's parser takes.

use super::{
    CLOCK_RATE, Matrix, Stream,
    audio::TrackKind,
    mux::{DecoderConfig, NAL_LENGTH_BYTES},
};
use crate::config::{Codec, Container};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};
use thiserror::Error;

mod mkv;
mod mp4;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// A temporal delimiter OBU with its size field, which is zero.
const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];
/// Matrix coefficients (H.273) that are BT.601's.
const BT601_MATRICES: [u16; 2] = [5, 6];
const MAX_FRAME_RATE: u32 = 1000;

#[derive(Debug, Error)]
pub(crate) enum DemuxError {
    #[error("read the clip")]
    Io(#[from] io::Error),
    #[error("the clip is not an MP4 or MKV file")]
    Container,
    #[error("the clip's index is damaged")]
    Index(#[from] mp4_atom::Error),
    #[error("the clip's Matroska structure is damaged")]
    Matroska,
    #[error("the clip has no H.264, HEVC or AV1 video")]
    NoVideo,
    #[error("the clip's video has no frames")]
    Empty,
}

/// Where a sample's bytes are in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) offset: u64,
    pub(crate) size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoSample {
    pub(crate) place: Place,
    /// When the picture is shown, in clock ticks from the first picture shown.
    pub(crate) time: i64,
    pub(crate) keyframe: bool,
}

pub(crate) struct Video {
    pub(crate) stream: Stream,
    config: DecoderConfig,
    /// In decode order.
    pub(crate) samples: Vec<VideoSample>,
    /// When the last picture ends, in clock ticks.
    pub(crate) duration: i64,
}

/// An AAC-LC track of frames of [`FRAME_SAMPLES`](super::FRAME_SAMPLES) samples each.
pub(crate) struct Sound {
    pub(crate) kind: TrackKind,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The `AudioSpecificConfig`.
    pub(crate) config: Box<[u8]>,
    pub(crate) frames: Vec<Place>,
    /// The sample at which frame 0 starts, counted from the first picture shown. Negative when
    /// it starts earlier.
    pub(crate) start: i64,
}

pub(crate) struct Movie {
    pub(crate) video: Video,
    pub(crate) sounds: Vec<Sound>,
}

impl Movie {
    pub(crate) fn open(path: &Path) -> Result<(Self, File), DemuxError> {
        let container = path
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(Container::of_extension)
            .ok_or(DemuxError::Container)?;
        let mut file = File::open(path)?;
        let movie = Self::read(&mut file, container)?;
        Ok((movie, file))
    }

    pub(crate) fn read(
        file: &mut (impl Read + Seek),
        container: Container,
    ) -> Result<Self, DemuxError> {
        let (video, sounds) = match container {
            Container::Mp4 => mp4::read(file)?,
            Container::Mkv => mkv::read(file)?,
        };
        if video.samples.is_empty() {
            return Err(DemuxError::Empty);
        }
        Ok(Self { video, sounds })
    }
}

pub(crate) fn read_place(file: &mut (impl Read + Seek), place: Place) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; place.size as usize];
    file.seek(SeekFrom::Start(place.offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl Video {
    /// Returns the sample as the decoder's parser reads it: H.264 and HEVC in Annex B, AV1 as a
    /// temporal unit. A keyframe carries the parameter sets or sequence header ahead of it, so
    /// decoding may start there.
    pub(crate) fn decoder_input(&self, sample: &[u8], keyframe: bool) -> Vec<u8> {
        let mut input = Vec::with_capacity(sample.len() + 64);
        match &self.config {
            DecoderConfig::Avc(avcc) => {
                if keyframe {
                    for unit in avcc
                        .sequence_parameter_sets
                        .iter()
                        .chain(&avcc.picture_parameter_sets)
                    {
                        input.extend_from_slice(&START_CODE);
                        input.extend_from_slice(unit);
                    }
                }
                annex_b(&mut input, sample, avcc.length_size);
            }
            DecoderConfig::Hevc(hvcc) => {
                if keyframe {
                    for unit in hvcc.arrays.iter().flat_map(|array| &array.nalus) {
                        input.extend_from_slice(&START_CODE);
                        input.extend_from_slice(unit);
                    }
                }
                annex_b(&mut input, sample, hvcc.length_size_minus_one + 1);
            }
            DecoderConfig::Av1(av1c) => {
                input.extend_from_slice(&TEMPORAL_DELIMITER);
                if keyframe {
                    input.extend_from_slice(&av1c.config_obus);
                }
                input.extend_from_slice(sample);
            }
        }
        input
    }

    /// The index of the last keyframe at or before `time` in decode order, from which decoding
    /// reaches the picture shown then.
    pub(crate) fn keyframe_before(&self, time: i64) -> usize {
        self.samples
            .iter()
            .rposition(|sample| sample.keyframe && sample.time <= time)
            .or_else(|| self.samples.iter().position(|sample| sample.keyframe))
            .unwrap_or(0)
    }
}

#[cfg(test)]
impl Video {
    /// An H.264 track of `samples` at 1920×1080.
    pub(crate) fn of_samples(frame_rate: u32, samples: Vec<VideoSample>) -> Self {
        let duration = samples.iter().map(|sample| sample.time).max().unwrap_or(0)
            + CLOCK_RATE / i64::from(frame_rate);
        Self {
            stream: Stream {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                frame_rate,
                matrix: Matrix::Bt709,
            },
            config: DecoderConfig::Avc(mp4_atom::Avcc::default()),
            samples,
            duration,
        }
    }
}

/// Appends the length-prefixed units in `sample` with start codes instead. A unit whose length
/// overruns the sample ends it.
fn annex_b(output: &mut Vec<u8>, mut sample: &[u8], length_size: u8) {
    let length_size = usize::from(if (1..=4).contains(&length_size) {
        length_size
    } else {
        NAL_LENGTH_BYTES
    });
    while sample.len() > length_size {
        let (length, rest) = sample.split_at(length_size);
        let length = length
            .iter()
            .fold(0usize, |value, &byte| (value << 8) | usize::from(byte));
        let Some(unit) = rest.get(..length) else {
            return;
        };
        output.extend_from_slice(&START_CODE);
        output.extend_from_slice(unit);
        sample = &rest[length..];
    }
}

/// The video track as either container describes it, before its samples are ordered.
struct VideoTrack {
    codec: Codec,
    config: DecoderConfig,
    width: u32,
    height: u32,
    matrix: Matrix,
}

/// Times are in clock ticks, possibly offset; `finish` counts them from the first picture
/// shown.
fn finish_video(
    track: VideoTrack,
    mut samples: Vec<VideoSample>,
    frame_ticks: i64,
) -> Result<(Video, i64), DemuxError> {
    let first = samples
        .iter()
        .map(|sample| sample.time)
        .min()
        .ok_or(DemuxError::Empty)?;
    for sample in &mut samples {
        sample.time -= first;
    }
    let frame_ticks = frame_ticks.max(1);
    let last = samples.iter().map(|sample| sample.time).max().unwrap_or(0);
    let frame_rate = u32::try_from((CLOCK_RATE + frame_ticks / 2) / frame_ticks)
        .unwrap_or(MAX_FRAME_RATE)
        .clamp(1, MAX_FRAME_RATE);
    let video = Video {
        stream: Stream {
            codec: track.codec,
            width: track.width,
            height: track.height,
            frame_rate,
            matrix: track.matrix,
        },
        config: track.config,
        samples,
        duration: last + frame_ticks,
    };
    Ok((video, first))
}

fn matrix_of(code: u16) -> Matrix {
    if BT601_MATRICES.contains(&code) {
        Matrix::Bt601
    } else {
        Matrix::Bt709
    }
}

fn track_kind(name: &str) -> TrackKind {
    [TrackKind::Mixed, TrackKind::Microphone, TrackKind::System]
        .into_iter()
        .find(|kind| kind.name() == name)
        .unwrap_or(TrackKind::System)
}

/// The sample at `ticks` at `rate`, rounded to the nearest.
fn nearest_sample(ticks: i64, rate: u32) -> i64 {
    let scaled = i128::from(ticks) * i128::from(rate);
    let clock = i128::from(CLOCK_RATE);
    i64::try_from((scaled + scaled.signum() * clock / 2) / clock).unwrap_or(0)
}

#[cfg(test)]
mod tests;
