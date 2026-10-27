//! A minimal Matroska writer: one H.264, HEVC or AV1 video track and optionally one AAC track.
//! The first pass over the packets sizes every element, so the file is written front to back
//! with its seek head and cues, and the packets are passed over twice, as for MP4.

use super::{
    AudioClip, FRAME_SAMPLES, Stream,
    mp4::{self, DecoderConfig, MuxError, Packets, Sample},
    sample_parts,
};
use crate::config::Codec;
use mp4_atom::Encode;
use std::io;

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const LANGUAGE: u32 = 0x22_B59C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const COLOUR: u32 = 0x55B0;
const MATRIX_COEFFICIENTS: u32 = 0x55B1;
const RANGE: u32 = 0x55B9;
const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const PRIMARIES: u32 = 0x55BB;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;

/// Timestamps count milliseconds.
const NANOSECONDS_PER_TICK: u64 = 1_000_000;
const TICKS_PER_SECOND: u64 = 1000;
const VIDEO_TRACK: u8 = 1;
const AUDIO_TRACK: u8 = 2;
const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_TYPE_AUDIO: u64 = 2;
/// A cluster ends at the next keyframe, or this long after it began. Block times are 16-bit
/// offsets from the cluster's.
const MAX_CLUSTER_TICKS: u64 = 5000;
/// A block's track number, as a one-byte size, then its 16-bit time and its flags.
const BLOCK_HEADER_BYTES: u64 = 4;
const KEYFRAME_FLAG: u8 = 0x80;
/// Master elements whose size depends on the media take an eight-byte size, and positions are
/// written in eight bytes, so no size depends on the values written.
const WIDE: usize = 8;
const BT709: u64 = 1;
const BROADCAST_RANGE: u64 = 1;
const UNDETERMINED_LANGUAGE: &str = "und";

pub(crate) fn write_from<P: Packets + ?Sized>(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &P,
    audio: Option<&AudioClip>,
) -> Result<(), MuxError> {
    let (decoder, samples) = mp4::index(stream.codec, packets)?;
    let audio = audio.filter(|clip| !clip.frames.is_empty());
    let layout = Layout::of(stream, &samples, audio)?;
    let head = Head::of(stream, &decoder, audio, &layout)?;

    output.write_all(&head.bytes)?;
    let mut writer = BlockWriter {
        output,
        stream,
        samples: &samples,
        audio,
        items: layout.items.iter(),
        written: 0,
    };
    packets.visit(&mut |_, _, data| writer.video_block(data))?;
    writer.finish()?;
    output.write_all(&layout.cues(head.clusters_at))?;
    output.flush()?;
    Ok(())
}

/// What the media part of the file holds, in order.
enum Item {
    Cluster { time: u64, body: u64 },
    Video { sample: usize, offset: i16 },
    Audio { frame: usize, offset: i16 },
}

struct Layout {
    items: Vec<Item>,
    /// Each keyframe's time and the position of its cluster among the clusters' bytes.
    cues: Vec<(u64, u64)>,
    clusters_bytes: u64,
    duration: u64,
}

/// A block before it is placed in a cluster.
struct Block {
    time: u64,
    track: u8,
    number: usize,
    bytes: u64,
    keyframe: bool,
}

impl Layout {
    fn of(
        stream: &Stream,
        samples: &[Sample],
        audio: Option<&AudioClip>,
    ) -> Result<Self, MuxError> {
        // Sound starts before the video by the clip's lead, and a time cannot be negative, so the
        // video starts that much later instead.
        let lead = audio.map_or(0, |clip| {
            ticks(u64::from(clip.lead), u64::from(clip.sample_rate))
        });
        let first = samples.first().map_or(0, |sample| sample.index);
        let frame_rate = u64::from(stream.frame_rate);
        let video = samples.iter().enumerate().map(|(number, sample)| Block {
            time: lead + ticks(sample.index - first, frame_rate),
            track: VIDEO_TRACK,
            number,
            bytes: u64::from(sample.size),
            keyframe: sample.keyframe,
        });
        let sound = audio.into_iter().flat_map(|clip| {
            clip.frames.iter().enumerate().map(|(number, frame)| Block {
                time: ticks(
                    number as u64 * u64::from(FRAME_SAMPLES),
                    u64::from(clip.sample_rate),
                ),
                track: AUDIO_TRACK,
                number,
                bytes: frame.len() as u64,
                keyframe: true,
            })
        });
        let blocks = merge(video.collect(), sound.collect());
        let duration = samples
            .last()
            .map_or(0, |last| lead + ticks(last.index - first + 1, frame_rate));
        Self::place(blocks, duration)
    }

    fn place(blocks: Vec<Block>, duration: u64) -> Result<Self, MuxError> {
        let mut items = Vec::with_capacity(blocks.len() + blocks.len() / 30 + 1);
        let mut cues = Vec::new();
        let mut clusters_bytes = 0;
        let mut open: Option<Cluster> = None;
        for block in blocks {
            let mut cluster = match open.take() {
                Some(cluster) if !cluster.ends_before(&block) => cluster,
                finished => {
                    if let Some(done) = finished {
                        clusters_bytes += done.close(&mut items);
                    }
                    items.push(Item::Cluster {
                        time: block.time,
                        body: 0,
                    });
                    Cluster::open(items.len() - 1, block.time)
                }
            };
            let video = block.track == VIDEO_TRACK;
            if video && block.keyframe {
                cues.push((block.time, clusters_bytes));
            }
            let offset =
                i16::try_from(block.time - cluster.time).map_err(|_| MuxError::TooLarge)?;
            cluster.body += element_bytes(SIMPLE_BLOCK, BLOCK_HEADER_BYTES + block.bytes);
            cluster.has_video |= video;
            items.push(if video {
                Item::Video {
                    sample: block.number,
                    offset,
                }
            } else {
                Item::Audio {
                    frame: block.number,
                    offset,
                }
            });
            open = Some(cluster);
        }
        if let Some(done) = open {
            clusters_bytes += done.close(&mut items);
        }
        Ok(Self {
            items,
            cues,
            clusters_bytes,
            duration,
        })
    }

    /// The cues element, with cluster positions counted from the segment's data.
    fn cues(&self, clusters_at: u64) -> Vec<u8> {
        master(
            CUES,
            &self
                .cues
                .iter()
                .map(|&(time, cluster)| {
                    master(
                        CUE_POINT,
                        &[
                            uint(CUE_TIME, time),
                            master(
                                CUE_TRACK_POSITIONS,
                                &[
                                    uint(CUE_TRACK, u64::from(VIDEO_TRACK)),
                                    wide_uint(CUE_CLUSTER_POSITION, clusters_at + cluster),
                                ],
                            ),
                        ],
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    fn cues_bytes(&self) -> u64 {
        self.cues(0).len() as u64
    }
}

/// A cluster being filled while the layout is planned.
struct Cluster {
    /// Its place among the items.
    at: usize,
    time: u64,
    body: u64,
    has_video: bool,
}

impl Cluster {
    fn open(at: usize, time: u64) -> Self {
        Self {
            at,
            time,
            body: uint(TIMESTAMP, time).len() as u64,
            has_video: false,
        }
    }

    /// A keyframe starts the next cluster, so each cue points at the cluster it begins.
    fn ends_before(&self, block: &Block) -> bool {
        (block.track == VIDEO_TRACK && block.keyframe && self.has_video)
            || block.time - self.time >= MAX_CLUSTER_TICKS
    }

    /// Records the body size in the cluster's item and returns the cluster's size as an element.
    fn close(self, items: &mut [Item]) -> u64 {
        if let Some(Item::Cluster { body, .. }) = items.get_mut(self.at) {
            *body = self.body;
        }
        id_bytes(CLUSTER) + WIDE as u64 + self.body
    }
}

/// Merges by time, video first at the same time.
fn merge(video: Vec<Block>, audio: Vec<Block>) -> Vec<Block> {
    let mut merged = Vec::with_capacity(video.len() + audio.len());
    let mut audio = audio.into_iter().peekable();
    for block in video {
        while let Some(earlier) = audio.next_if(|sound| sound.time < block.time) {
            merged.push(earlier);
        }
        merged.push(block);
    }
    merged.extend(audio);
    merged
}

/// `count` units of `1 / rate` seconds in ticks, rounded to the nearest.
fn ticks(count: u64, rate: u64) -> u64 {
    (count * TICKS_PER_SECOND + rate / 2) / rate.max(1)
}

/// Everything before the clusters, and where the clusters start within the segment's data.
struct Head {
    bytes: Vec<u8>,
    clusters_at: u64,
}

impl Head {
    fn of(
        stream: &Stream,
        decoder: &DecoderConfig,
        audio: Option<&AudioClip>,
        layout: &Layout,
    ) -> Result<Self, MuxError> {
        let info = info(layout.duration);
        let tracks = tracks(stream, decoder, audio)?;
        let seek_head_bytes = seek_head(0, 0, 0).len() as u64;
        let info_at = seek_head_bytes;
        let tracks_at = info_at + info.len() as u64;
        let clusters_at = tracks_at + tracks.len() as u64;
        let cues_at = clusters_at + layout.clusters_bytes;
        let segment_bytes = cues_at + layout.cues_bytes();

        let mut bytes = ebml_header();
        bytes.extend(id(SEGMENT));
        bytes.extend(wide_size(segment_bytes));
        bytes.extend(seek_head(info_at, tracks_at, cues_at));
        bytes.extend(info);
        bytes.extend(tracks);
        Ok(Self { bytes, clusters_at })
    }
}

fn ebml_header() -> Vec<u8> {
    master(
        EBML,
        &[
            uint(EBML_VERSION, 1),
            uint(EBML_READ_VERSION, 1),
            uint(EBML_MAX_ID_LENGTH, 4),
            uint(EBML_MAX_SIZE_LENGTH, 8),
            string(DOC_TYPE, "matroska"),
            uint(DOC_TYPE_VERSION, 4),
            uint(DOC_TYPE_READ_VERSION, 2),
        ],
    )
}

fn seek_head(info_at: u64, tracks_at: u64, cues_at: u64) -> Vec<u8> {
    let seek = |target: u32, at: u64| {
        master(
            SEEK,
            &[element(SEEK_ID, &id(target)), wide_uint(SEEK_POSITION, at)],
        )
    };
    master(
        SEEK_HEAD,
        &[
            seek(INFO, info_at),
            seek(TRACKS, tracks_at),
            seek(CUES, cues_at),
        ],
    )
}

fn info(duration: u64) -> Vec<u8> {
    master(
        INFO,
        &[
            uint(TIMESTAMP_SCALE, NANOSECONDS_PER_TICK),
            float(DURATION, duration as f64),
            string(MUXING_APP, env!("CARGO_PKG_NAME")),
            string(
                WRITING_APP,
                concat!("hindsight ", env!("CARGO_PKG_VERSION")),
            ),
        ],
    )
}

fn tracks(
    stream: &Stream,
    decoder: &DecoderConfig,
    audio: Option<&AudioClip>,
) -> Result<Vec<u8>, MuxError> {
    let mut entries = vec![video_entry(stream, decoder)?];
    if let Some(clip) = audio {
        entries.push(audio_entry(clip));
    }
    Ok(master(TRACKS, &entries))
}

fn video_entry(stream: &Stream, decoder: &DecoderConfig) -> Result<Vec<u8>, MuxError> {
    let (codec_id, record) = match decoder {
        DecoderConfig::Avc(avcc) => ("V_MPEG4/ISO/AVC", body(avcc)?),
        DecoderConfig::Hevc(hvcc) => ("V_MPEGH/ISO/HEVC", body(hvcc)?),
        DecoderConfig::Av1(av1c) => ("V_AV1", body(av1c)?),
    };
    let frame_nanoseconds = 1_000_000_000 / u64::from(stream.frame_rate).max(1);
    Ok(master(
        TRACK_ENTRY,
        &[
            uint(TRACK_NUMBER, u64::from(VIDEO_TRACK)),
            uint(TRACK_UID, u64::from(VIDEO_TRACK)),
            uint(TRACK_TYPE, TRACK_TYPE_VIDEO),
            uint(FLAG_LACING, 0),
            string(LANGUAGE, UNDETERMINED_LANGUAGE),
            string(CODEC_ID, codec_id),
            element(CODEC_PRIVATE, &record),
            uint(DEFAULT_DURATION, frame_nanoseconds),
            master(
                VIDEO,
                &[
                    uint(PIXEL_WIDTH, u64::from(stream.width)),
                    uint(PIXEL_HEIGHT, u64::from(stream.height)),
                    master(
                        COLOUR,
                        &[
                            uint(MATRIX_COEFFICIENTS, stream.matrix.code().into()),
                            uint(RANGE, BROADCAST_RANGE),
                            uint(TRANSFER_CHARACTERISTICS, BT709),
                            uint(PRIMARIES, BT709),
                        ],
                    ),
                ],
            ),
        ],
    ))
}

fn audio_entry(clip: &AudioClip) -> Vec<u8> {
    master(
        TRACK_ENTRY,
        &[
            uint(TRACK_NUMBER, u64::from(AUDIO_TRACK)),
            uint(TRACK_UID, u64::from(AUDIO_TRACK)),
            uint(TRACK_TYPE, TRACK_TYPE_AUDIO),
            uint(FLAG_LACING, 0),
            string(LANGUAGE, UNDETERMINED_LANGUAGE),
            string(CODEC_ID, "A_AAC"),
            element(CODEC_PRIVATE, &clip.config),
            master(
                AUDIO,
                &[
                    float(SAMPLING_FREQUENCY, f64::from(clip.sample_rate)),
                    uint(CHANNELS, u64::from(clip.channels)),
                ],
            ),
        ],
    )
}

/// A decoder configuration record without its MP4 box header, as `CodecPrivate` holds it.
fn body(record: &impl Encode) -> Result<Vec<u8>, MuxError> {
    let mut boxed = Vec::new();
    record.encode(&mut boxed)?;
    Ok(boxed.split_off(8))
}

struct BlockWriter<'a, W> {
    output: &'a mut W,
    stream: &'a Stream,
    samples: &'a [Sample],
    audio: Option<&'a AudioClip>,
    items: std::slice::Iter<'a, Item>,
    written: usize,
}

impl<W: io::Write> BlockWriter<'_, W> {
    fn video_block(&mut self, data: &[u8]) -> Result<(), MuxError> {
        let codec = self.stream.codec;
        let parts = sample_parts(codec, data);
        if parts.is_empty() {
            return Ok(());
        }
        let sample = self.samples.get(self.written).ok_or(MuxError::Changed)?;
        if sample.size != mp4::stored_size(&parts, mp4::prefix_bytes(codec))? {
            return Err(MuxError::Changed);
        }
        let offset = loop {
            match self.items.next() {
                Some(Item::Video { sample, offset }) if *sample == self.written => break *offset,
                Some(Item::Video { .. }) | None => return Err(MuxError::Changed),
                Some(other) => self.write_item(other)?,
            }
        };
        self.block_header(VIDEO_TRACK, offset, sample.keyframe, u64::from(sample.size))?;
        for part in parts {
            if length_prefixed(codec) {
                let length: u32 = part.len().try_into().map_err(|_| MuxError::TooLarge)?;
                self.output.write_all(&length.to_be_bytes())?;
            }
            self.output.write_all(part)?;
        }
        self.written += 1;
        Ok(())
    }

    fn write_item(&mut self, item: &Item) -> Result<(), MuxError> {
        match *item {
            Item::Cluster { time, body } => {
                self.output.write_all(&id(CLUSTER))?;
                self.output.write_all(&wide_size(body))?;
                self.output.write_all(&uint(TIMESTAMP, time))?;
            }
            Item::Audio { frame, offset } => {
                let data = self
                    .audio
                    .and_then(|clip| clip.frames.get(frame))
                    .ok_or(MuxError::Changed)?;
                self.block_header(AUDIO_TRACK, offset, true, data.len() as u64)?;
                self.output.write_all(data)?;
            }
            Item::Video { .. } => return Err(MuxError::Changed),
        }
        Ok(())
    }

    fn block_header(
        &mut self,
        track: u8,
        offset: i16,
        keyframe: bool,
        bytes: u64,
    ) -> Result<(), MuxError> {
        self.output.write_all(&id(SIMPLE_BLOCK))?;
        self.output.write_all(&size(BLOCK_HEADER_BYTES + bytes))?;
        let flags = if keyframe { KEYFRAME_FLAG } else { 0 };
        self.output.write_all(&[0x80 | track])?;
        self.output.write_all(&offset.to_be_bytes())?;
        self.output.write_all(&[flags])?;
        Ok(())
    }

    fn finish(mut self) -> Result<(), MuxError> {
        if self.written != self.samples.len() {
            return Err(MuxError::Changed);
        }
        while let Some(item) = self.items.next() {
            self.write_item(item)?;
        }
        Ok(())
    }
}

/// H.264 and HEVC blocks are length-prefixed NAL units; AV1 OBUs carry their own sizes.
const fn length_prefixed(codec: Codec) -> bool {
    !matches!(codec, Codec::Av1)
}

/// An element ID's bytes. IDs carry their own length marker, so leading zero bytes are dropped.
fn id(value: u32) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(3);
    bytes[first..].to_vec()
}

fn id_bytes(value: u32) -> u64 {
    id(value).len() as u64
}

/// A data size in the fewest bytes that hold it. All ones is reserved for unknown sizes.
fn size(value: u64) -> Vec<u8> {
    let length = (1..WIDE)
        .find(|&length| value < (1 << (7 * length)) - 1)
        .unwrap_or(WIDE);
    sized(value, length)
}

/// A data size in eight bytes.
fn wide_size(value: u64) -> Vec<u8> {
    sized(value, WIDE)
}

fn sized(value: u64, length: usize) -> Vec<u8> {
    let marked = value | (1 << (7 * length));
    marked.to_be_bytes()[WIDE - length..].to_vec()
}

/// The size of an element with a `body`-byte body.
fn element_bytes(element_id: u32, body: u64) -> u64 {
    id_bytes(element_id) + size(body).len() as u64 + body
}

fn element(element_id: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = id(element_id);
    bytes.extend(size(body.len() as u64));
    bytes.extend_from_slice(body);
    bytes
}

fn master(element_id: u32, children: &[Vec<u8>]) -> Vec<u8> {
    element(element_id, &children.concat())
}

fn uint_value(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(WIDE - 1);
    bytes[first..].to_vec()
}

fn uint(element_id: u32, value: u64) -> Vec<u8> {
    element(element_id, &uint_value(value))
}

fn wide_uint(element_id: u32, value: u64) -> Vec<u8> {
    element(element_id, &value.to_be_bytes())
}

fn float(element_id: u32, value: f64) -> Vec<u8> {
    element(element_id, &value.to_be_bytes())
}

fn string(element_id: u32, value: &str) -> Vec<u8> {
    element(element_id, value.as_bytes())
}

#[cfg(test)]
mod tests;
