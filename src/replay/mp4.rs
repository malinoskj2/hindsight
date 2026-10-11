//! A minimal MP4 writer for one video track in H.264, HEVC or AV1 and, when there is sound, one
//! AAC track. The index (`moov`) is written before the media data, so players can start a clip
//! without reading to its end; with every packet already in memory, the offsets are known up
//! front.
//!
//! Video alone sits in one chunk. With sound, the media data alternates a second of video with
//! the second of sound that plays alongside it, so a player reads both from one place in the
//! file; the sound's edit list skips the samples of its first frames that play before the video
//! starts, so the two start together.

use super::{AudioClip, FRAME_SAMPLES, Packet, Stream, configuration, sample_parts};
use crate::config::Codec;
use std::{io, sync::Arc};
use thiserror::Error;

/// Media ticks per frame; the track's time scale is the frame rate times this.
const TICKS_PER_FRAME: u32 = 1000;
/// The movie header's time scale: milliseconds.
const MOVIE_TIMESCALE: u32 = 1000;
const TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;
/// How much of each track's media one interleaved chunk holds, in seconds.
const CHUNK_SECONDS: u64 = 1;
/// The `esds` descriptor tags (ISO/IEC 14496-1): the elementary stream, its decoder
/// configuration, the decoder-specific information and the sync layer configuration.
const ES_DESCRIPTOR: u8 = 0x03;
const DECODER_CONFIG_DESCRIPTOR: u8 = 0x04;
const DECODER_SPECIFIC_INFO: u8 = 0x05;
const SL_CONFIG_DESCRIPTOR: u8 = 0x06;
/// The object type of MPEG-4 audio (AAC), and the stream type of audio shifted past the
/// upstream flag, with the reserved bit set.
const OBJECT_TYPE_MPEG4_AUDIO: u8 = 0x40;
const STREAM_TYPE_AUDIO: u8 = (0x05 << 2) | 1;
/// The sync layer configuration MP4 files use.
const SL_PREDEFINED_MP4: u8 = 0x02;
/// The bits of a sample that an `mp4a` entry declares; AAC decodes to 16-bit samples.
const AUDIO_SAMPLE_BITS: u16 = 16;
/// The bytes of the length in front of each H.264 or HEVC NAL unit in a sample, which the
/// `avcC` and `hvcC` records declare.
pub(super) const NAL_LENGTH_BYTES: u8 = 4;
/// An atom size of 1 means a 64-bit size follows the atom type.
const LARGE_SIZE: u32 = 1;
/// A 16-byte `mdat` header with a 64-bit size, so clips larger than 4 GB need no special case.
const MDAT_HEADER_BYTES: u64 = 16;
/// 1.0 in 16.16 fixed point: the playback rate, and the matrix's scale factors.
const FIXED_ONE: u32 = 0x0001_0000;
/// The identity transform, with its last column in 2.30 fixed point.
const UNITY_MATRIX: [u32; 9] = [FIXED_ONE, 0, 0, 0, FIXED_ONE, 0, 0, 0, 0x4000_0000];
/// 1.0 in 8.8 fixed point: full volume.
const FULL_VOLUME: u16 = 0x0100;
/// `ftyp` minor version, as other muxers write it next to the `isom` brand.
const FILE_TYPE_MINOR_VERSION: u32 = 0x200;
/// `tkhd` flags: the track is enabled and part of the presentation.
const TRACK_ENABLED_IN_MOVIE: u32 = 0x3;
/// The `vmhd` flags the specification requires.
const VIDEO_MEDIA_HEADER_FLAGS: u32 = 1;
/// The `url ` flag saying the media is in this file.
const SELF_CONTAINED: u32 = 1;
/// `und`, packed as three 5-bit letters.
const UNDETERMINED_LANGUAGE: u16 = 0x55C4;
/// 72 dpi in 16.16 fixed point, the resolution every visual sample entry declares.
const RESOLUTION_72_DPI: u32 = 0x0048_0000;
/// 24-bit colour without alpha.
const DEPTH_24_BIT: u16 = 0x0018;
/// BT.709 primaries, transfer and matrix, limited range: what the encoder signals.
const BT709: u16 = 1;

/// One packet as the index describes it and the media data stores it.
struct Sample<'a> {
    /// The packet's frame index.
    index: u64,
    parts: Vec<&'a [u8]>,
    /// The bytes `parts` take in the media data, lengths included.
    size: u32,
    /// Media ticks until the next sample.
    duration: u32,
    keyframe: bool,
}

/// What the index describes: the stream, its decoder configuration record and its samples.
struct Track<'a> {
    stream: &'a Stream,
    decoder: &'a [u8],
    samples: &'a [Sample<'a>],
}

/// The track's lengths in time: its media time scale, and its duration in that scale and in the
/// movie's.
struct Durations {
    timescale: u32,
    media: u32,
    movie: u32,
}

impl Durations {
    /// The time scale from the frame rate, and the sum of the sample durations in it and in
    /// milliseconds.
    fn of(track: &Track<'_>) -> Result<Self, MuxError> {
        let timescale = track
            .stream
            .frame_rate
            .checked_mul(TICKS_PER_FRAME)
            .ok_or(MuxError::TooLarge)?;
        let media: u64 = track
            .samples
            .iter()
            .map(|sample| u64::from(sample.duration))
            .sum();
        let movie = media * u64::from(MOVIE_TIMESCALE) / u64::from(timescale);
        Ok(Self {
            timescale,
            media: fit(media)?,
            movie: fit(movie)?,
        })
    }
}

/// What the index describes of the sound: the clip's frames, all [`FRAME_SAMPLES`] long, and
/// their sizes.
struct Sound<'a> {
    clip: &'a AudioClip,
    sizes: Vec<u32>,
}

impl<'a> Sound<'a> {
    fn of(clip: &'a AudioClip) -> Result<Self, MuxError> {
        let sizes = clip
            .frames
            .iter()
            .map(|frame| fit(frame.len()))
            .collect::<Result<_, _>>()?;
        Ok(Self { clip, sizes })
    }

    /// The sound's length in its own time scale, the sample rate.
    fn media_duration(&self) -> Result<u32, MuxError> {
        fit(self.sizes.len() as u64 * u64::from(FRAME_SAMPLES))
    }

    /// The bitrate over the whole clip and the highest over any second of it, in bits per
    /// second, for the decoder configuration.
    fn bitrates(&self) -> (u32, u32) {
        let frames_per_second = (self.clip.format.sample_rate / FRAME_SAMPLES).max(1) as usize;
        let bits = |sizes: &[u32]| sizes.iter().map(|&size| u64::from(size) * 8).sum::<u64>();
        let seconds = (self.sizes.len() as u64 * u64::from(FRAME_SAMPLES))
            .div_ceil(u64::from(self.clip.format.sample_rate.max(1)))
            .max(1);
        let average = bits(&self.sizes) / seconds;
        let peak = self
            .sizes
            .windows(frames_per_second.min(self.sizes.len()).max(1))
            .map(bits)
            .max()
            .unwrap_or(0);
        let clamp = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
        (clamp(average), clamp(peak.max(average)))
    }
}

/// Which track a chunk belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
}

/// A run of consecutive samples of one track, stored together in the media data.
struct Chunk {
    kind: Kind,
    /// The track's sample the chunk starts with, and how many follow it.
    first: usize,
    count: usize,
    /// The bytes the chunk takes in the media data.
    bytes: u64,
}

/// The chunks in the order the media data stores them.
struct Layout {
    chunks: Vec<Chunk>,
}

impl Layout {
    /// Video alone in one chunk, or video and sound alternating a chunk of
    /// [`CHUNK_SECONDS`] each, the video's first.
    fn of(track: &Track<'_>, sound: Option<&Sound<'_>>) -> Self {
        let Some(sound) = sound else {
            return Self {
                chunks: vec![Chunk {
                    kind: Kind::Video,
                    first: 0,
                    count: track.samples.len(),
                    bytes: track
                        .samples
                        .iter()
                        .map(|sample| u64::from(sample.size))
                        .sum(),
                }],
            };
        };
        let first_index = track.samples.first().map_or(0, |sample| sample.index);
        let frame_rate = u64::from(track.stream.frame_rate.max(1));
        let video = runs(
            Kind::Video,
            track.samples.iter().map(|sample| {
                let window = (sample.index - first_index) / frame_rate / CHUNK_SECONDS;
                (window, u64::from(sample.size))
            }),
        );
        let rate = u64::from(sound.clip.format.sample_rate.max(1));
        let lead = u64::from(sound.clip.lead);
        let audio = runs(
            Kind::Audio,
            (0..).zip(&sound.sizes).map(|(number, &size)| {
                let start = (number * u64::from(FRAME_SAMPLES)).saturating_sub(lead);
                (start / rate / CHUNK_SECONDS, u64::from(size))
            }),
        );
        Self {
            chunks: interleave(video, audio),
        }
    }

    /// Where `kind`'s chunks are in a file whose media data starts at `media_offset`.
    fn placement(&self, kind: Kind, media_offset: u64) -> Result<Placement, MuxError> {
        let mut placement = Placement {
            counts: Vec::new(),
            offsets: Vec::new(),
        };
        let mut offset = media_offset;
        for chunk in &self.chunks {
            if chunk.kind == kind {
                placement.counts.push(fit(chunk.count)?);
                placement.offsets.push(offset);
            }
            offset += chunk.bytes;
        }
        Ok(placement)
    }

    /// The bytes of the media data.
    fn bytes(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.bytes).sum()
    }
}

/// Where one track's chunks are: how many samples each holds, and its offset in the file.
struct Placement {
    counts: Vec<u32>,
    offsets: Vec<u64>,
}

/// Groups a track's samples, given as (window, bytes) pairs in order, into one chunk per
/// window, each tagged with its window.
fn runs(kind: Kind, samples: impl Iterator<Item = (u64, u64)>) -> Vec<(u64, Chunk)> {
    let mut chunks: Vec<(u64, Chunk)> = Vec::new();
    for (sample, (window, bytes)) in samples.enumerate() {
        match chunks.last_mut() {
            Some((last, chunk)) if *last == window => {
                chunk.count += 1;
                chunk.bytes += bytes;
            }
            _ => chunks.push((
                window,
                Chunk {
                    kind,
                    first: sample,
                    count: 1,
                    bytes,
                },
            )),
        }
    }
    chunks
}

/// Merges two tracks' chunks by window, the video's first within a window.
fn interleave(video: Vec<(u64, Chunk)>, audio: Vec<(u64, Chunk)>) -> Vec<Chunk> {
    let mut merged = Vec::with_capacity(video.len() + audio.len());
    let mut audio = audio.into_iter().peekable();
    for (window, chunk) in video {
        while let Some((_, earlier)) = audio.next_if(|(other, _)| *other < window) {
            merged.push(earlier);
        }
        merged.push(chunk);
    }
    merged.extend(audio.map(|(_, chunk)| chunk));
    merged
}

/// Why a clip could not be written.
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

/// Writes `packets`, oldest first and starting on a keyframe, as a complete MP4 file.
pub(crate) fn write(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
) -> Result<(), MuxError> {
    write_tracks(output, stream, packets, None)
}

/// Writes `packets`, oldest first and starting on a keyframe, and the sound `audio` that plays
/// alongside them, as a complete MP4 file.
pub(crate) fn write_with_audio(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
    audio: &AudioClip,
) -> Result<(), MuxError> {
    write_tracks(output, stream, packets, Some(audio))
}

/// Writes the video track of `packets` and, when there is one, the sound track of `audio`.
fn write_tracks(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
    audio: Option<&AudioClip>,
) -> Result<(), MuxError> {
    let decoder = packets
        .iter()
        .find_map(|packet| configuration(stream.codec, &packet.data))
        .ok_or(MuxError::NoSequenceHeader)?;
    let samples = samples(stream.codec, packets)?;
    if samples.is_empty() {
        return Err(MuxError::Empty);
    }
    let track = Track {
        stream,
        decoder: &decoder,
        samples: &samples,
    };
    let sound = audio
        .filter(|clip| !clip.frames.is_empty())
        .map(Sound::of)
        .transpose()?;
    let layout = Layout::of(&track, sound.as_ref());

    output.write_all(&header(&track, sound.as_ref(), &layout)?)?;
    write_media(output, &track, sound.as_ref(), &layout)?;
    output.flush()?;
    Ok(())
}

/// Each packet as a sample; a frame index that skips ahead lengthens the previous sample.
fn samples(codec: Codec, packets: &[Arc<Packet>]) -> Result<Vec<Sample<'_>>, MuxError> {
    let prefix = if length_prefixed(codec) {
        usize::from(NAL_LENGTH_BYTES)
    } else {
        0
    };
    let mut samples: Vec<Sample<'_>> = Vec::with_capacity(packets.len());
    for packet in packets {
        let parts = sample_parts(codec, &packet.data);
        if parts.is_empty() {
            continue;
        }
        let size = stored_size(&parts, prefix)?;
        if let Some(last) = samples.last_mut() {
            last.duration = duration_between(last.index, packet.index)?;
        }
        samples.push(Sample {
            index: packet.index,
            parts,
            size,
            duration: TICKS_PER_FRAME,
            keyframe: packet.keyframe,
        });
    }
    Ok(samples)
}

/// The bytes `parts` take in the media data, each with a `prefix`-byte length in front.
fn stored_size(parts: &[&[u8]], prefix: usize) -> Result<u32, MuxError> {
    parts
        .iter()
        .map(|part| fit::<u32, _>(part.len() + prefix))
        .sum()
}

/// The media ticks a sample at frame `from` lasts until the one at frame `to`, at least a frame.
fn duration_between(from: u64, to: u64) -> Result<u32, MuxError> {
    let frames: u32 = fit(to.saturating_sub(from).max(1))?;
    frames
        .checked_mul(TICKS_PER_FRAME)
        .ok_or(MuxError::TooLarge)
}

/// The `ftyp` and `moov` atoms, with the chunk offsets counted from just past them and the
/// `mdat` header, where the media starts.
fn header(
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
) -> Result<Vec<u8>, MuxError> {
    let file_type = file_type(track.stream.codec);
    // Offsets are 64-bit whatever their value, so the index has the same length either way.
    let index_length = movie(track, sound, layout, 0)?.len();
    let media_offset = (file_type.len() + index_length) as u64 + MDAT_HEADER_BYTES;
    let index = movie(track, sound, layout, media_offset)?;
    Ok([file_type, index].concat())
}

/// Writes the `mdat` atom: its 64-bit size, then the chunks in order: every video sample's
/// parts, each behind its length when the codec stores lengths, and the sound frames as they
/// are.
fn write_media(
    output: &mut impl io::Write,
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
) -> Result<(), MuxError> {
    let prefixed = length_prefixed(track.stream.codec);

    output.write_all(&LARGE_SIZE.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(&(MDAT_HEADER_BYTES + layout.bytes()).to_be_bytes())?;
    for chunk in &layout.chunks {
        let range = chunk.first..chunk.first + chunk.count;
        match (chunk.kind, sound) {
            (Kind::Video, _) => {
                for part in track.samples[range].iter().flat_map(|sample| &sample.parts) {
                    if prefixed {
                        let length: u32 = fit(part.len())?;
                        output.write_all(&length.to_be_bytes())?;
                    }
                    output.write_all(part)?;
                }
            }
            (Kind::Audio, Some(sound)) => {
                for frame in &sound.clip.frames[range] {
                    output.write_all(frame)?;
                }
            }
            (Kind::Audio, None) => {}
        }
    }
    Ok(())
}

/// H.264 and HEVC samples are NAL units behind four-byte lengths; AV1 OBUs carry their own.
const fn length_prefixed(codec: Codec) -> bool {
    !matches!(codec, Codec::Av1)
}

/// `value` as the narrower integer an MP4 field holds, or [`MuxError::TooLarge`].
fn fit<T: TryFrom<U>, U>(value: U) -> Result<T, MuxError> {
    T::try_from(value).map_err(|_| MuxError::TooLarge)
}

/// The `ftyp` atom: the brands a player checks before reading on.
fn file_type(codec: Codec) -> Vec<u8> {
    let codec_brand: &[u8] = match codec {
        Codec::H264 => b"avc1",
        // There is no widely used HEVC-specific brand; `isom`, `iso2` and `mp41` are what
        // players check.
        Codec::Hevc => b"",
        Codec::Av1 => b"av01",
    };
    let mut body = Vec::new();
    // The major brand and its version, then the compatible brands.
    body.put(b"isom")
        .put_u32(FILE_TYPE_MINOR_VERSION)
        .put(b"isom")
        .put(b"iso2")
        .put(codec_brand)
        .put(b"mp41");
    atom(*b"ftyp", &body)
}

/// The `moov` atom: the movie header, the video track and the sound track if there is one, with
/// the media data starting at `media_offset`.
fn movie(
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
    media_offset: u64,
) -> Result<Vec<u8>, MuxError> {
    let durations = Durations::of(track)?;
    let video = layout.placement(Kind::Video, media_offset)?;
    let track_atoms = [
        track_header(track.stream, durations.movie),
        media(track, &durations, &video)?,
    ]
    .concat();
    let mut body = [
        movie_header(durations.movie, sound.is_some()),
        atom(*b"trak", &track_atoms),
    ]
    .concat();
    if let Some(sound) = sound {
        let audio = layout.placement(Kind::Audio, media_offset)?;
        body.extend(sound_track(sound, durations.movie, &audio)?);
    }
    Ok(atom(*b"moov", &body))
}

/// The `mvhd` atom: the movie's time scale and duration, and the next free track id, which
/// follows the sound track's when `with_sound`.
fn movie_header(duration: u32, with_sound: bool) -> Vec<u8> {
    let next_track = if with_sound { AUDIO_TRACK_ID } else { TRACK_ID } + 1;
    let mut body = Vec::new();
    body
        // Creation and modification times.
        .put_zeros(8)
        .put_u32(MOVIE_TIMESCALE)
        .put_u32(duration)
        .put_u32(FIXED_ONE)
        .put_u16(FULL_VOLUME)
        .put_zeros(10)
        .put_u32s(&UNITY_MATRIX)
        // Pre-defined fields.
        .put_zeros(24)
        .put_u32(next_track);
    full_atom(*b"mvhd", 0, 0, &body)
}

/// The `tkhd` atom: the track's id, its duration in the movie's time scale and its size.
fn track_header(stream: &Stream, duration: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body
        // Creation and modification times.
        .put_zeros(8)
        .put_u32(TRACK_ID)
        .put_zeros(4)
        .put_u32(duration)
        // Reserved, layer, alternate group, volume (none for video), reserved.
        .put_zeros(16)
        .put_u32s(&UNITY_MATRIX)
        // Width and height in 16.16 fixed point.
        .put_u32(stream.width << 16)
        .put_u32(stream.height << 16);
    full_atom(*b"tkhd", 0, TRACK_ENABLED_IN_MOVIE, &body)
}

/// The `mdia` atom: the media's timing, its handler and the media information.
fn media(
    track: &Track<'_>,
    durations: &Durations,
    placement: &Placement,
) -> Result<Vec<u8>, MuxError> {
    Ok(atom(
        *b"mdia",
        &[
            media_header(durations.timescale, durations.media),
            handler(*b"vide", b"VideoHandler\0"),
            media_information(track, placement)?,
        ]
        .concat(),
    ))
}

/// The `mdhd` atom: the media's time scale and its duration in it.
fn media_header(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body
        // Creation and modification times.
        .put_zeros(8)
        .put_u32(timescale)
        .put_u32(duration)
        .put_u16(UNDETERMINED_LANGUAGE)
        .put_zeros(2);
    full_atom(*b"mdhd", 0, 0, &body)
}

/// The `hdlr` atom: what kind of media the track holds (`vide` or `soun`), and its name.
fn handler(kind: [u8; 4], name: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.put_zeros(4).put(&kind).put_zeros(12).put(name);
    full_atom(*b"hdlr", 0, 0, &body)
}

/// The `minf` atom: the video media header, where the data is, and the sample table.
fn media_information(track: &Track<'_>, placement: &Placement) -> Result<Vec<u8>, MuxError> {
    Ok(atom(
        *b"minf",
        &[
            full_atom(*b"vmhd", 0, VIDEO_MEDIA_HEADER_FLAGS, &[0; 8]),
            data_information(),
            sample_table(track, placement)?,
        ]
        .concat(),
    ))
}

/// The `dinf` atom: one data reference, saying the media is in this file.
fn data_information() -> Vec<u8> {
    let mut table = Vec::new();
    table
        .put_u32(1)
        .put(&full_atom(*b"url ", 0, SELF_CONTAINED, &[]));
    atom(*b"dinf", &full_atom(*b"dref", 0, 0, &table))
}

/// The `stbl` atom: the sample description, then where each sample sits in time and in the file.
fn sample_table(track: &Track<'_>, placement: &Placement) -> Result<Vec<u8>, MuxError> {
    let count: u32 = fit(track.samples.len())?;
    let sizes: Vec<u32> = track.samples.iter().map(|sample| sample.size).collect();
    Ok(atom(
        *b"stbl",
        &[
            sample_description(track)?,
            time_to_sample(track.samples.iter().map(|sample| sample.duration))?,
            sync_samples(track.samples, count)?,
            sample_to_chunk(&placement.counts)?,
            sample_sizes(&sizes, count),
            chunk_offsets(&placement.offsets)?,
        ]
        .concat(),
    ))
}

/// The `stsd` atom: the one sample description.
fn sample_description(track: &Track<'_>) -> Result<Vec<u8>, MuxError> {
    let mut body = Vec::new();
    body.put_u32(1)
        .put(&visual_entry(track.stream, track.decoder)?);
    Ok(full_atom(*b"stsd", 0, 0, &body))
}

/// The visual sample entry (`avc1`, `hvc1` or `av01`): the picture's size, then the decoder
/// configuration record and the colour.
fn visual_entry(stream: &Stream, decoder: &[u8]) -> Result<Vec<u8>, MuxError> {
    let width: u16 = fit(stream.width)?;
    let height: u16 = fit(stream.height)?;
    // Parameter sets live only in the configuration record (`hvc1`, not `hev1`).
    let kind = match stream.codec {
        Codec::H264 => b"avc1",
        Codec::Hevc => b"hvc1",
        Codec::Av1 => b"av01",
    };

    let mut body = Vec::new();
    body.put_zeros(6)
        // Data reference index.
        .put_u16(1)
        .put_zeros(16)
        .put_u16(width)
        .put_u16(height)
        .put_u32(RESOLUTION_72_DPI)
        .put_u32(RESOLUTION_72_DPI)
        .put_zeros(4)
        // Frames per sample.
        .put_u16(1)
        // Compressor name.
        .put_zeros(32)
        .put_u16(DEPTH_24_BIT)
        // Pre-defined.
        .put_u16(u16::MAX)
        .put(decoder)
        .put(&colour());
    Ok(atom(*kind, &body))
}

/// The `colr` atom: the colour the encoder signals.
fn colour() -> Vec<u8> {
    let mut body = Vec::new();
    // Primaries, transfer and matrix, then the full-range flag cleared.
    body.put(b"nclx").put_u16s(&[BT709; 3]).put(&[0]);
    atom(*b"colr", &body)
}

/// The `stts` atom: the sample durations, as runs of equal ones.
fn time_to_sample(durations: impl IntoIterator<Item = u32>) -> Result<Vec<u8>, MuxError> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for next in durations {
        match runs.last_mut() {
            Some((run, duration)) if *duration == next => *run += 1,
            _ => runs.push((1, next)),
        }
    }
    let mut body = Vec::new();
    body.put_u32(fit(runs.len())?);
    for (run, duration) in runs {
        body.put_u32(run).put_u32(duration);
    }
    Ok(full_atom(*b"stts", 0, 0, &body))
}

/// The `stss` atom: the keyframes among the `count` samples, by sample number from 1.
fn sync_samples(samples: &[Sample<'_>], count: u32) -> Result<Vec<u8>, MuxError> {
    let keyframes: Vec<u32> = (1..=count)
        .zip(samples)
        .filter(|(_, sample)| sample.keyframe)
        .map(|(number, _)| number)
        .collect();
    let mut body = Vec::new();
    body.put_u32(fit(keyframes.len())?).put_u32s(&keyframes);
    Ok(full_atom(*b"stss", 0, 0, &body))
}

/// The `stsc` atom: how many samples each chunk holds, as one entry per run of chunks that
/// hold the same number (the first chunk of the run, its count, description 1). Video alone is
/// one entry for its one chunk.
fn sample_to_chunk(counts: &[u32]) -> Result<Vec<u8>, MuxError> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (number, &count) in (1..).zip(counts) {
        if runs.last().is_none_or(|&(_, previous)| previous != count) {
            runs.push((number, count));
        }
    }
    let mut body = Vec::new();
    body.put_u32(fit(runs.len())?);
    for (first, count) in runs {
        body.put_u32s(&[first, count, 1]);
    }
    Ok(full_atom(*b"stsc", 0, 0, &body))
}

/// The `stsz` atom: a default size of 0, so each of the `count` samples' sizes is listed.
fn sample_sizes(sizes: &[u32], count: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.put_u32(0).put_u32(count).put_u32s(sizes);
    full_atom(*b"stsz", 0, 0, &body)
}

/// The `co64` atom: each chunk's offset in the file.
fn chunk_offsets(offsets: &[u64]) -> Result<Vec<u8>, MuxError> {
    let mut body = Vec::new();
    body.put_u32(fit(offsets.len())?);
    for &offset in offsets {
        body.put_u64(offset);
    }
    Ok(full_atom(*b"co64", 0, 0, &body))
}

/// The sound's `trak` atom: its header, the edit that starts it with the video and lasts
/// `duration` in the movie's time scale, and its media, whose chunks sit as `placement` says.
fn sound_track(
    sound: &Sound<'_>,
    duration: u32,
    placement: &Placement,
) -> Result<Vec<u8>, MuxError> {
    let format = &sound.clip.format;
    let media = atom(
        *b"mdia",
        &[
            media_header(format.sample_rate, sound.media_duration()?),
            handler(*b"soun", b"SoundHandler\0"),
            atom(
                *b"minf",
                &[
                    // Balance (centred) and reserved.
                    full_atom(*b"smhd", 0, 0, &[0; 4]),
                    data_information(),
                    sound_sample_table(sound, placement)?,
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    Ok(atom(
        *b"trak",
        &[
            sound_track_header(duration),
            edit(duration, sound.clip.lead),
            media,
        ]
        .concat(),
    ))
}

/// The sound's `tkhd` atom: its id, its duration in the movie's time scale and full volume.
fn sound_track_header(duration: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body
        // Creation and modification times.
        .put_zeros(8)
        .put_u32(AUDIO_TRACK_ID)
        .put_zeros(4)
        .put_u32(duration)
        // Reserved, layer and alternate group.
        .put_zeros(12)
        .put_u16(FULL_VOLUME)
        .put_zeros(2)
        .put_u32s(&UNITY_MATRIX)
        // No width or height.
        .put_zeros(8);
    full_atom(*b"tkhd", 0, TRACK_ENABLED_IN_MOVIE, &body)
}

/// The `edts` atom with one edit: the track plays for `duration` in the movie's time scale,
/// from `skip` samples into its media at normal speed.
fn edit(duration: u32, skip: u32) -> Vec<u8> {
    let mut list = Vec::new();
    list.put_u32(1)
        .put_u32(duration)
        .put_u32(skip)
        // Media rate 1.0, in 16.16 fixed point.
        .put_u32(FIXED_ONE);
    atom(*b"edts", &full_atom(*b"elst", 0, 0, &list))
}

/// The sound's `stbl` atom: its sample description, then every frame lasting
/// [`FRAME_SAMPLES`], and where each sits in the file. Every AAC frame decodes on its own, so
/// there is no `stss`.
fn sound_sample_table(sound: &Sound<'_>, placement: &Placement) -> Result<Vec<u8>, MuxError> {
    let count: u32 = fit(sound.sizes.len())?;
    let mut description = Vec::new();
    description.put_u32(1).put(&audio_entry(sound)?);
    Ok(atom(
        *b"stbl",
        &[
            full_atom(*b"stsd", 0, 0, &description),
            time_to_sample(sound.sizes.iter().map(|_| FRAME_SAMPLES))?,
            sample_to_chunk(&placement.counts)?,
            sample_sizes(&sound.sizes, count),
            chunk_offsets(&placement.offsets)?,
        ]
        .concat(),
    ))
}

/// The `mp4a` sample entry: the channels, the sample size and rate, then the `esds` atom.
fn audio_entry(sound: &Sound<'_>) -> Result<Vec<u8>, MuxError> {
    let format = &sound.clip.format;
    // The rate is 16.16 fixed point, so it must fit 16 bits.
    let rate: u16 = fit(format.sample_rate)?;
    let mut body = Vec::new();
    body.put_zeros(6)
        // Data reference index.
        .put_u16(1)
        // Version, revision and vendor.
        .put_zeros(8)
        .put_u16(format.channels)
        .put_u16(AUDIO_SAMPLE_BITS)
        // Pre-defined and reserved.
        .put_zeros(4)
        .put_u32(u32::from(rate) << 16)
        .put(&elementary_stream(sound)?);
    Ok(atom(*b"mp4a", &body))
}

/// The `esds` atom: the elementary stream descriptor, which says the stream is AAC, gives its
/// buffer size and bitrates, and carries the `AudioSpecificConfig`.
fn elementary_stream(sound: &Sound<'_>) -> Result<Vec<u8>, MuxError> {
    let (average, peak) = sound.bitrates();
    let largest = sound.sizes.iter().copied().max().unwrap_or(0);
    let mut decoder = Vec::new();
    decoder
        .put(&[OBJECT_TYPE_MPEG4_AUDIO, STREAM_TYPE_AUDIO])
        // The buffer size in 24 bits.
        .put(&largest.min(0x00FF_FFFF).to_be_bytes()[1..])
        .put_u32(peak)
        .put_u32(average)
        .put(&descriptor(
            DECODER_SPECIFIC_INFO,
            &sound.clip.format.config,
        )?);
    let mut stream = Vec::new();
    stream
        // Elementary stream id (0 in MP4 files) and flags.
        .put_u16(0)
        .put(&[0])
        .put(&descriptor(DECODER_CONFIG_DESCRIPTOR, &decoder)?)
        .put(&descriptor(SL_CONFIG_DESCRIPTOR, &[SL_PREDEFINED_MP4])?);
    Ok(full_atom(
        *b"esds",
        0,
        0,
        &descriptor(ES_DESCRIPTOR, &stream)?,
    ))
}

/// An MPEG-4 descriptor: its tag, its length in one byte (all of these are short), then `body`.
fn descriptor(tag: u8, body: &[u8]) -> Result<Vec<u8>, MuxError> {
    let length = u8::try_from(body.len())
        .ok()
        .filter(|&length| length < 0x80)
        .ok_or(MuxError::TooLarge)?;
    Ok([&[tag, length][..], body].concat())
}

/// An atom: its 32-bit size, its type, then `body`.
pub(super) fn atom(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    // Index atoms stay far below 4 GB; only `mdat` can grow past it, and it is written apart.
    let size = u32::try_from(body.len() + 8).expect("an index atom fits a 32-bit size");
    [&size.to_be_bytes()[..], &kind, body].concat()
}

/// A full atom: an atom whose body starts with a version byte and 24 bits of flags.
fn full_atom(kind: [u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let header = (u32::from(version) << 24) | (flags & 0x00FF_FFFF);
    atom(kind, &[&header.to_be_bytes()[..], body].concat())
}

/// Appends a parameter set to an `avcC` or `hvcC` record behind its 16-bit length, unless it is
/// too long for one.
pub(super) fn write_parameter_set(record: &mut Vec<u8>, unit: &[u8]) -> Option<()> {
    record.put_u16(u16::try_from(unit.len()).ok()?).put(unit);
    Some(())
}

/// Big-endian field writers for building atom bodies.
trait Fields {
    fn put(&mut self, bytes: &[u8]) -> &mut Self;
    fn put_zeros(&mut self, count: usize) -> &mut Self;
    fn put_u16(&mut self, value: u16) -> &mut Self;
    fn put_u32(&mut self, value: u32) -> &mut Self;
    fn put_u64(&mut self, value: u64) -> &mut Self;

    fn put_u16s(&mut self, values: &[u16]) -> &mut Self {
        values.iter().fold(self, |body, &value| body.put_u16(value))
    }

    fn put_u32s(&mut self, values: &[u32]) -> &mut Self {
        values.iter().fold(self, |body, &value| body.put_u32(value))
    }
}

impl Fields for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) -> &mut Self {
        self.extend_from_slice(bytes);
        self
    }

    fn put_zeros(&mut self, count: usize) -> &mut Self {
        self.resize(self.len() + count, 0);
        self
    }

    fn put_u16(&mut self, value: u16) -> &mut Self {
        self.put(&value.to_be_bytes())
    }

    fn put_u32(&mut self, value: u32) -> &mut Self {
        self.put(&value.to_be_bytes())
    }

    fn put_u64(&mut self, value: u64) -> &mut Self {
        self.put(&value.to_be_bytes())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::replay::{av1, h264, hevc};
    use std::assert_matches;

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

    /// The single chunk's offset, from the `co64` atom in `table`.
    fn chunk_offset(table: &[u8]) -> usize {
        let offsets = child(table, *b"co64");
        usize::try_from(u64::from_be_bytes(offsets[8..16].try_into().unwrap())).unwrap()
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

        let offset = chunk_offset(table);
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
    fn hevc_gets_an_hvc1_entry_and_length_prefixed_samples() {
        let packets = [packet(Codec::Hevc, 0, true), packet(Codec::Hevc, 1, false)];
        let file = mux(Codec::Hevc, &packets);

        let table = sample_table(&file);
        let visual = child(&child(table, *b"stsd")[8..], *b"hvc1");
        child(&visual[78..], *b"hvcC");
        let sizes = words(child(table, *b"stsz"));
        assert_eq!(sizes[3] as usize, hevc::tests::IDR.len() + 4);
    }

    #[test]
    fn av1_gets_its_brand_an_av01_entry_and_whole_obus_as_samples() {
        let packets = [packet(Codec::Av1, 0, true), packet(Codec::Av1, 1, false)];
        let file = mux(Codec::Av1, &packets);

        let file_type = atoms(&file)[0].1;
        assert!(file_type.as_chunks::<4>().0.contains(b"av01"));
        let table = sample_table(&file);
        let visual = child(&child(table, *b"stsd")[8..], *b"av01");
        child(&visual[78..], *b"av1C");

        // AV1 samples are the OBUs as they are, minus the temporal delimiter.
        let kept: usize = av1::sample_obus(&packets[0].data)
            .iter()
            .map(|obu| obu.len())
            .sum();
        let sizes = words(child(table, *b"stsz"));
        assert_eq!(sizes[3] as usize, kept);
        let offset = chunk_offset(table);
        assert_eq!(
            file[offset] >> 3,
            1,
            "the sample starts with the sequence header"
        );
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

    /// The `trak` atoms of `file`'s movie.
    fn tracks(file: &[u8]) -> Vec<&[u8]> {
        atoms(atoms(file)[1].1)
            .into_iter()
            .filter(|(kind, _)| kind == b"trak")
            .map(|(_, body)| body)
            .collect()
    }

    fn table_of(track: &[u8]) -> &[u8] {
        child(child(child(track, *b"mdia"), *b"minf"), *b"stbl")
    }

    /// The offsets in a `co64` atom's body.
    fn offsets(body: &[u8]) -> Vec<usize> {
        body[8..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|offset| usize::try_from(u64::from_be_bytes(*offset)).unwrap())
            .collect()
    }

    /// The sum of the sample durations in an `stts` atom's body.
    fn total_duration(body: &[u8]) -> u32 {
        words(&body[8..])
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[count, duration]| count * duration)
            .sum()
    }

    #[test]
    fn sound_gets_its_own_track_interleaved_with_the_video() {
        // Two seconds of video at 60 fps, and 100 sound frames of distinct sizes and contents.
        let packets: Vec<_> = (0..120)
            .map(|index| packet(Codec::H264, index, index % 30 == 0))
            .collect();
        let frames: Vec<Arc<[u8]>> = (0..100_u8)
            .map(|number| Arc::from(vec![number; 10 + usize::from(number % 7)]))
            .collect();
        let clip = AudioClip {
            format: crate::replay::audio::tests::format(1024),
            frames: frames.clone(),
            lead: 1500,
        };
        let mut file = Vec::new();
        write_with_audio(&mut file, &stream(Codec::H264), &packets, &clip).unwrap();

        // The movie lasts as long as the video, and the next track id follows the sound's.
        let header = child(atoms(&file)[1].1, *b"mvhd");
        assert_eq!(words(&header[12..20]), [1000, 2000]);
        assert_eq!(words(&header[96..100]), [3]);
        let [video, sound] = tracks(&file)[..] else {
            panic!("expected two tracks");
        };

        // The video is described as before, split into a chunk per second.
        let video_table = table_of(video);
        assert_eq!(words(&child(video_table, *b"stsz")[8..12]), [120]);
        assert_eq!(total_duration(child(video_table, *b"stts")), 120 * 1000);
        assert_eq!(words(child(video_table, *b"stsc")), [0, 1, 1, 60, 1]);

        // The sound: its handler, time scale, length, edit, frames and description.
        let media = child(sound, *b"mdia");
        assert_eq!(&child(media, *b"hdlr")[8..12], b"soun");
        assert_eq!(words(&child(media, *b"mdhd")[12..20]), [48_000, 100 * 1024]);
        assert_eq!(words(&child(sound, *b"tkhd")[20..24]), [2000]);
        let edits = child(child(sound, *b"edts"), *b"elst");
        assert_eq!(words(edits), [0, 1, 2000, 1500, 0x0001_0000]);
        let sound_table = table_of(sound);
        assert_eq!(words(child(sound_table, *b"stts")), [0, 1, 100, 1024]);
        let sizes = words(child(sound_table, *b"stsz"));
        assert_eq!(sizes[..3], [0, 0, 100]);
        assert!(
            sizes[3..]
                .iter()
                .zip(&frames)
                .all(|(&size, frame)| size as usize == frame.len())
        );
        // Frames start each second of playback from the end of the lead: 49, 47 and 4 of them.
        assert_eq!(
            words(child(sound_table, *b"stsc")),
            [0, 3, 1, 49, 1, 2, 47, 1, 3, 4, 1]
        );
        let entry = child(&child(sound_table, *b"stsd")[8..], *b"mp4a");
        assert_eq!(&entry[16..18], &2_u16.to_be_bytes());
        assert_eq!(words(&entry[24..28]), [48_000 << 16]);
        let descriptor = child(&entry[28..], *b"esds");
        assert!(descriptor.ends_with(&[0x05, 2, 0x11, 0x90, 0x06, 1, 0x02]));

        // The chunks alternate, a second of video then the sound that plays with it, and each
        // offset points at its first sample's bytes.
        let video_offsets = offsets(child(video_table, *b"co64"));
        let sound_offsets = offsets(child(sound_table, *b"co64"));
        assert!(video_offsets[0] < sound_offsets[0]);
        assert!(sound_offsets[0] < video_offsets[1]);
        assert!(video_offsets[1] < sound_offsets[1]);
        assert!(sound_offsets[1] < sound_offsets[2]);
        for (offset, first) in sound_offsets.iter().zip([0, 49, 96]) {
            assert_eq!(
                &file[*offset..*offset + frames[first].len()],
                &*frames[first]
            );
        }
        for offset in video_offsets {
            assert_eq!(&file[offset + 4..offset + 9], &h264::tests::IDR);
        }
        let media_end =
            sound_offsets[2] + frames[96..].iter().map(|frame| frame.len()).sum::<usize>();
        assert_eq!(media_end, file.len());
    }

    #[test]
    fn refuses_streams_without_a_sequence_header() {
        let mut file = Vec::new();
        for codec in Codec::ALL {
            assert_matches!(
                write(&mut file, &stream(codec), &[packet(codec, 0, false)]),
                Err(MuxError::NoSequenceHeader)
            );
        }
        assert_matches!(
            write(&mut file, &stream(Codec::H264), &[]),
            Err(MuxError::NoSequenceHeader)
        );
        assert_eq!(file.len(), 0);
    }
}
