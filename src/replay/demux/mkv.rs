//! The Matroska reader: the tracks, read whole, and the clusters, walked block by block without
//! reading the frames themselves.

use super::{
    DemuxError, Place, Sound, Video, VideoSample, VideoTrack, finish_video, matrix_of,
    nearest_sample, track_kind,
};
use crate::{
    config::Codec,
    replay::{Matrix, mux::DecoderConfig},
};
use mp4_atom::{Atom, Av1c, Avcc, Hvcc};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Seek, SeekFrom},
};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const NAME: u32 = 0x536E;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const COLOUR: u32 = 0x55B0;
const MATRIX_COEFFICIENTS: u32 = 0x55B1;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
/// The elements that can follow a cluster of unknown size, ending it.
const TOP_LEVEL: [u32; 8] = [
    CLUSTER,
    INFO,
    TRACKS,
    0x114D_9B74,
    0x1C53_BB6B,
    0x1254_C367,
    0x1043_A770,
    0x1941_A469,
];

const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_TYPE_AUDIO: u64 = 2;
const KEYFRAME_FLAG: u8 = 0x80;
const LACING_MASK: u8 = 0x06;
const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;
const NANOSECONDS_PER_TICK: u64 = 100;
const MAX_VINT_BYTES: u32 = 8;
/// Larger than any track list or block group header a clip has.
const MAX_ELEMENT_BYTES: u64 = 16 << 20;

pub(super) fn read(file: &mut (impl Read + Seek)) -> Result<(Video, Vec<Sound>), DemuxError> {
    let end = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    let mut reader = Reader {
        input: BufReader::new(file),
        at: 0,
        end,
    };
    let header = reader.header()?.ok_or(DemuxError::Container)?;
    if header.id != EBML {
        return Err(DemuxError::Container);
    }
    reader.skip(header.size.ok_or(DemuxError::Matroska)?)?;
    let segment = reader.header()?.ok_or(DemuxError::Container)?;
    if segment.id != SEGMENT {
        return Err(DemuxError::Container);
    }
    let segment_end = segment.end().unwrap_or(end).min(end);
    let mut contents = Contents::default();
    let mut pending = None;
    while let Some(element) = pending
        .take()
        .map_or_else(|| reader.header_before(segment_end), |e| Ok(Some(e)))?
    {
        match element.id {
            INFO => {
                let body = reader.body(&element)?;
                if let Some(scale) = child(&body, TIMESTAMP_SCALE)? {
                    contents.timestamp_scale = number(scale);
                }
            }
            TRACKS => contents.tracks = tracks(&reader.body(&element)?)?,
            CLUSTER => pending = contents.cluster(&mut reader, &element, segment_end)?,
            _ => reader.skip_element(&element, segment_end)?,
        }
    }
    contents.finish()
}

#[derive(Clone, Copy)]
struct Header {
    id: u32,
    /// `None` when the size is unknown.
    size: Option<u64>,
    /// Where the element's body starts.
    at: u64,
}

impl Header {
    fn end(&self) -> Option<u64> {
        self.size.map(|size| self.at.saturating_add(size))
    }
}

struct Reader<R> {
    input: BufReader<R>,
    at: u64,
    end: u64,
}

impl<R: Read + Seek> Reader<R> {
    fn byte(&mut self) -> Result<u8, DemuxError> {
        let mut byte = [0];
        self.input.read_exact(&mut byte)?;
        self.at += 1;
        Ok(byte[0])
    }

    /// Reads a variable-length integer and returns it, with its marker bit when `keep_marker`,
    /// and whether every value bit is set, which marks an unknown size.
    fn vint(&mut self, keep_marker: bool) -> Result<(u64, bool), DemuxError> {
        let first = self.byte()?;
        let length = first.leading_zeros() + 1;
        if length > MAX_VINT_BYTES {
            return Err(DemuxError::Matroska);
        }
        let marker = 0x80u8 >> (length - 1);
        let mut value = u64::from(if keep_marker { first } else { first & !marker });
        let mut all_ones = first & (marker - 1) == marker - 1;
        for _ in 1..length {
            let byte = self.byte()?;
            all_ones &= byte == 0xFF;
            value = (value << 8) | u64::from(byte);
        }
        Ok((value, all_ones && !keep_marker))
    }

    fn header(&mut self) -> Result<Option<Header>, DemuxError> {
        if self.at >= self.end {
            return Ok(None);
        }
        let (id, _) = self.vint(true)?;
        let (size, unknown) = self.vint(false)?;
        Ok(Some(Header {
            id: u32::try_from(id).map_err(|_| DemuxError::Matroska)?,
            size: (!unknown).then_some(size),
            at: self.at,
        }))
    }

    fn header_before(&mut self, end: u64) -> Result<Option<Header>, DemuxError> {
        if self.at >= end {
            return Ok(None);
        }
        self.header()
    }

    fn skip(&mut self, bytes: u64) -> Result<(), DemuxError> {
        let offset = i64::try_from(bytes).map_err(|_| DemuxError::Matroska)?;
        self.input.seek_relative(offset)?;
        self.at = self.at.saturating_add(bytes);
        Ok(())
    }

    fn skip_element(&mut self, element: &Header, end: u64) -> Result<(), DemuxError> {
        match element.size {
            Some(size) => self.skip(size),
            // Only a cluster may be of unknown size in practice; anything else runs to the end.
            None => self.skip(end.saturating_sub(self.at)),
        }
    }

    fn body(&mut self, element: &Header) -> Result<Vec<u8>, DemuxError> {
        let size = element
            .size
            .filter(|&size| size <= MAX_ELEMENT_BYTES)
            .ok_or(DemuxError::Matroska)?;
        let mut body = vec![0; usize::try_from(size).map_err(|_| DemuxError::Matroska)?];
        self.input.read_exact(&mut body)?;
        self.at += size;
        Ok(body)
    }
}

#[derive(Default)]
struct Contents {
    timestamp_scale: u64,
    tracks: Tracks,
    blocks: HashMap<u64, Vec<Block>>,
}

struct Block {
    place: Place,
    /// In units of the timestamp scale.
    time: i64,
    keyframe: bool,
}

impl Contents {
    /// Reads the cluster's blocks. Returns the element that ended a cluster of unknown size.
    fn cluster<R: Read + Seek>(
        &mut self,
        reader: &mut Reader<R>,
        cluster: &Header,
        segment_end: u64,
    ) -> Result<Option<Header>, DemuxError> {
        let end = cluster.end().unwrap_or(segment_end).min(segment_end);
        let mut time = 0i64;
        while let Some(element) = reader.header_before(end)? {
            if cluster.size.is_none() && TOP_LEVEL.contains(&element.id) {
                return Ok(Some(element));
            }
            match element.id {
                TIMESTAMP => {
                    time = i64::try_from(number(&reader.body(&element)?)).unwrap_or(i64::MAX);
                }
                SIMPLE_BLOCK => {
                    let size = element.size.ok_or(DemuxError::Matroska)?;
                    self.block(reader, size, time, None)?;
                }
                BLOCK_GROUP => self.block_group(reader, &element, time)?,
                _ => reader.skip_element(&element, end)?,
            }
        }
        Ok(None)
    }

    /// A block in a group is a keyframe unless the group references another block.
    fn block_group<R: Read + Seek>(
        &mut self,
        reader: &mut Reader<R>,
        group: &Header,
        cluster_time: i64,
    ) -> Result<(), DemuxError> {
        let end = group.end().ok_or(DemuxError::Matroska)?;
        let mut block = None;
        let mut references = false;
        while let Some(element) = reader.header_before(end)? {
            match element.id {
                BLOCK => {
                    let size = element.size.ok_or(DemuxError::Matroska)?;
                    block = self.block(reader, size, cluster_time, Some(true))?;
                }
                REFERENCE_BLOCK => {
                    references = true;
                    reader.skip_element(&element, end)?;
                }
                _ => reader.skip_element(&element, end)?,
            }
        }
        if references
            && let Some((track, index)) = block
            && let Some(block) = self
                .blocks
                .get_mut(&track)
                .and_then(|blocks| blocks.get_mut(index))
        {
            block.keyframe = false;
        }
        Ok(())
    }

    /// Records the block of `size` bytes at the reader. A laced block is skipped, since it packs
    /// several frames. Returns the block's track and position among its blocks.
    fn block<R: Read + Seek>(
        &mut self,
        reader: &mut Reader<R>,
        size: u64,
        cluster_time: i64,
        keyframe: Option<bool>,
    ) -> Result<Option<(u64, usize)>, DemuxError> {
        let start = reader.at;
        let (track, _) = reader.vint(false)?;
        let offset = i16::from_be_bytes([reader.byte()?, reader.byte()?]);
        let flags = reader.byte()?;
        let header = reader.at - start;
        let data = size.checked_sub(header).ok_or(DemuxError::Matroska)?;
        reader.skip(data)?;
        if flags & LACING_MASK != 0 {
            return Ok(None);
        }
        let blocks = self.blocks.entry(track).or_default();
        blocks.push(Block {
            place: Place {
                offset: start + header,
                size: u32::try_from(data).map_err(|_| DemuxError::Matroska)?,
            },
            time: cluster_time.saturating_add(i64::from(offset)),
            keyframe: keyframe.unwrap_or(flags & KEYFRAME_FLAG != 0),
        });
        Ok(Some((track, blocks.len() - 1)))
    }

    fn finish(mut self) -> Result<(Video, Vec<Sound>), DemuxError> {
        let scale = if self.timestamp_scale == 0 {
            DEFAULT_TIMESTAMP_SCALE
        } else {
            self.timestamp_scale
        };
        let ticks = |time: i64| {
            i64::try_from(i128::from(time) * i128::from(scale) / i128::from(NANOSECONDS_PER_TICK))
                .unwrap_or(i64::MAX)
        };
        let (number, video) = self.tracks.video.ok_or(DemuxError::NoVideo)?;
        let blocks = self.blocks.remove(&number).unwrap_or_default();
        let samples: Vec<VideoSample> = blocks
            .iter()
            .map(|block| VideoSample {
                place: block.place,
                time: ticks(block.time),
                keyframe: block.keyframe,
            })
            .collect();
        let frame_ticks = video.default_duration.map_or_else(
            || usual_step(&samples),
            |nanoseconds| i64::try_from(nanoseconds / NANOSECONDS_PER_TICK).unwrap_or(i64::MAX),
        );
        let (video, first) = finish_video(video.track, samples, frame_ticks)?;
        let sounds = self
            .tracks
            .audio
            .iter()
            .filter_map(|(number, audio)| {
                let blocks = self.blocks.get(number)?;
                let start = ticks(blocks.first()?.time).saturating_sub(first);
                Some(Sound {
                    kind: track_kind(&audio.name),
                    sample_rate: audio.sample_rate,
                    channels: audio.channels,
                    config: audio.config.clone(),
                    frames: blocks.iter().map(|block| block.place).collect(),
                    start: nearest_sample(start, audio.sample_rate),
                })
            })
            .collect();
        Ok((video, sounds))
    }
}

/// The commonest gap between successive pictures, in clock ticks.
fn usual_step(samples: &[VideoSample]) -> i64 {
    let mut times: Vec<i64> = samples.iter().map(|sample| sample.time).collect();
    times.sort_unstable();
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for pair in times.windows(2) {
        let step = pair[1] - pair[0];
        if step > 0 {
            *counts.entry(step).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|&(step, count)| (count, std::cmp::Reverse(step)))
        .map_or(1, |(step, _)| step)
}

/// The first video track and every sound track, each with its track number.
#[derive(Default)]
struct Tracks {
    video: Option<(u64, VideoEntry)>,
    audio: Vec<(u64, AudioEntry)>,
}

struct VideoEntry {
    track: VideoTrack,
    default_duration: Option<u64>,
}

struct AudioEntry {
    name: String,
    sample_rate: u32,
    channels: u16,
    config: Box<[u8]>,
}

/// The H.264, HEVC and AV1 video tracks and the AAC tracks. Others are left out.
fn tracks(body: &[u8]) -> Result<Tracks, DemuxError> {
    let mut found = Tracks::default();
    for (id, entry) in children(body)? {
        if id != TRACK_ENTRY {
            continue;
        }
        let fields: HashMap<u32, &[u8]> = children(entry)?.into_iter().collect();
        let field = |id| fields.get(&id).copied();
        let Some(track_number) = field(TRACK_NUMBER).map(number) else {
            continue;
        };
        let codec_id = field(CODEC_ID).map(text).unwrap_or_default();
        let private = field(CODEC_PRIVATE).unwrap_or_default();
        match field(TRACK_TYPE).map(number) {
            Some(TRACK_TYPE_VIDEO) if found.video.is_none() => {
                let Some((codec, config)) = video_codec(&codec_id, private) else {
                    continue;
                };
                let video = field(VIDEO).unwrap_or_default();
                let size = |id| {
                    child(video, id)
                        .ok()
                        .flatten()
                        .map_or(0, |value| u32::try_from(number(value)).unwrap_or(0))
                };
                let matrix = child(video, COLOUR)?
                    .and_then(|colour| child(colour, MATRIX_COEFFICIENTS).ok().flatten())
                    .map_or(Matrix::Bt709, |code| {
                        matrix_of(u16::try_from(number(code)).unwrap_or(0))
                    });
                let entry = VideoEntry {
                    track: VideoTrack {
                        codec,
                        config,
                        width: size(PIXEL_WIDTH),
                        height: size(PIXEL_HEIGHT),
                        matrix,
                    },
                    default_duration: field(DEFAULT_DURATION).map(number),
                };
                found.video = Some((track_number, entry));
            }
            Some(TRACK_TYPE_AUDIO) if codec_id == "A_AAC" && !private.is_empty() => {
                let audio = field(AUDIO).unwrap_or_default();
                let sample_rate = child(audio, SAMPLING_FREQUENCY)?.map_or(0.0, float);
                let channels = child(audio, CHANNELS)?.map_or(1, number);
                if sample_rate < 1.0 {
                    continue;
                }
                found.audio.push((
                    track_number,
                    AudioEntry {
                        name: field(NAME).map(text).unwrap_or_default(),
                        #[expect(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "a sampling frequency is a small positive whole number"
                        )]
                        sample_rate: sample_rate.round() as u32,
                        channels: u16::try_from(channels).unwrap_or(1),
                        config: private.into(),
                    },
                ));
            }
            _ => {}
        }
    }
    Ok(found)
}

fn video_codec(codec_id: &str, private: &[u8]) -> Option<(Codec, DecoderConfig)> {
    let mut record = private;
    Some(match codec_id {
        "V_MPEG4/ISO/AVC" => (
            Codec::H264,
            DecoderConfig::Avc(Avcc::decode_body(&mut record).ok()?),
        ),
        "V_MPEGH/ISO/HEVC" => (
            Codec::Hevc,
            DecoderConfig::Hevc(Hvcc::decode_body(&mut record).ok()?),
        ),
        "V_AV1" => (
            Codec::Av1,
            DecoderConfig::Av1(Av1c::decode_body(&mut record).ok()?),
        ),
        _ => return None,
    })
}

/// The elements in `body`, each as its id and body.
fn children(mut body: &[u8]) -> Result<Vec<(u32, &[u8])>, DemuxError> {
    let mut found = Vec::new();
    while !body.is_empty() {
        let (id, length) = vint(body, true)?;
        let (size, size_length) = vint(&body[length..], false)?;
        let start = length + size_length;
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .filter(|&end| end <= body.len())
            .ok_or(DemuxError::Matroska)?;
        found.push((
            u32::try_from(id).map_err(|_| DemuxError::Matroska)?,
            &body[start..end],
        ));
        body = &body[end..];
    }
    Ok(found)
}

fn child(body: &[u8], id: u32) -> Result<Option<&[u8]>, DemuxError> {
    Ok(children(body)?
        .into_iter()
        .find_map(|(found, value)| (found == id).then_some(value)))
}

fn vint(bytes: &[u8], keep_marker: bool) -> Result<(u64, usize), DemuxError> {
    let &first = bytes.first().ok_or(DemuxError::Matroska)?;
    let length = first.leading_zeros() as usize + 1;
    if length > MAX_VINT_BYTES as usize || bytes.len() < length {
        return Err(DemuxError::Matroska);
    }
    let marker = 0x80u8 >> (length - 1);
    let first = u64::from(if keep_marker { first } else { first & !marker });
    let value = bytes[1..length]
        .iter()
        .fold(first, |value, &byte| (value << 8) | u64::from(byte));
    Ok((value, length))
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

fn float(bytes: &[u8]) -> f64 {
    match bytes.len() {
        4 => f64::from(f32::from_bits(u32::try_from(number(bytes)).unwrap_or(0))),
        8 => f64::from_bits(number(bytes)),
        _ => 0.0,
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .to_owned()
}
