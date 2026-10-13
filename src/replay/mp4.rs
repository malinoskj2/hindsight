//! A minimal MP4 writer: one H.264, HEVC or AV1 video track and optionally one AAC track, with
//! `moov` ahead of `mdat`, so the packets are passed over twice.

#[cfg(test)]
use super::Packet;
use super::{AudioClip, FRAME_SAMPLES, StoreError, Stream, configuration, sample_parts};
use crate::config::Codec;
#[cfg(test)]
use mp4_atom::Decode;
use mp4_atom::{
    Audio, Av01, Av1c, Avc1, Avcc, Co64, Codec as SampleEntry, Colr, Dinf, Dref, Edts, Elst,
    ElstEntry, Encode, Esds, FourCC, Ftyp, Hdlr, Hvc1, Hvcc, Mdhd, Mdia, Minf, Moov, Mp4a, Mvhd,
    Smhd, Stbl, Stsc, StscEntry, Stsd, Stss, Stsz, StszSamples, Stts, SttsEntry, Tkhd, Trak, Url,
    Visual, Vmhd,
    esds::{DecoderConfig as EsDecoderConfig, DecoderSpecific, EsDescriptor, SLConfig},
    u24,
};
#[cfg(test)]
use std::sync::Arc;
use std::{io, slice};
use thiserror::Error;

/// Media ticks per frame; the track time scale is the frame rate times this.
const TICKS_PER_FRAME: u32 = 1000;
const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;
const CHUNK_SECONDS: u64 = 1;
/// `esds` object and stream types for AAC (ISO/IEC 14496-1).
const OBJECT_TYPE_MPEG4_AUDIO: u8 = 0x40;
const STREAM_TYPE_AUDIO: u8 = 0x05;
const AUDIO_SAMPLE_BITS: u16 = 16;
/// Length prefix on each NAL unit in a sample, as `avcC` and `hvcC` declare it.
pub(super) const NAL_LENGTH_BYTES: u8 = 4;
/// Atom size 1: a 64-bit size follows the type.
const LARGE_SIZE: u32 = 1;
/// `mdat` with a 64-bit size, so clips over 4 GB need no special case.
const MDAT_HEADER_BYTES: u64 = 16;
/// What other muxers write beside the `isom` brand.
const FILE_TYPE_MINOR_VERSION: u32 = 0x200;
const UNDETERMINED_LANGUAGE: &str = "und";
const DATA_REFERENCE: u16 = 1;
const BT709: u16 = 1;

struct Sample {
    index: u64,
    /// Bytes in the media data, length prefixes included.
    size: u32,
    /// Media ticks until the next sample.
    duration: u32,
    keyframe: bool,
}

/// The decoder configuration record a video sample entry carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecoderConfig {
    Avc(Avcc),
    Hevc(Hvcc),
    Av1(Av1c),
}

struct Track<'a> {
    stream: &'a Stream,
    decoder: &'a DecoderConfig,
    samples: &'a [Sample],
}

/// The track's duration in its media time scale and in the movie's.
struct Durations {
    timescale: u32,
    media: u32,
    movie: u32,
}

impl Durations {
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

    fn media_duration(&self) -> Result<u32, MuxError> {
        fit(self.sizes.len() as u64 * u64::from(FRAME_SAMPLES))
    }

    /// Average and peak bits per second, for the decoder configuration.
    fn bitrates(&self) -> (u32, u32) {
        let frames_per_second = (self.clip.sample_rate / FRAME_SAMPLES).max(1) as usize;
        let bits = |sizes: &[u32]| sizes.iter().map(|&size| u64::from(size) * 8).sum::<u64>();
        let seconds = (self.sizes.len() as u64 * u64::from(FRAME_SAMPLES))
            .div_ceil(u64::from(self.clip.sample_rate))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
}

struct Chunk {
    kind: Kind,
    first: usize,
    count: usize,
    bytes: u64,
}

struct Layout {
    chunks: Vec<Chunk>,
}

impl Layout {
    /// Video alone in one chunk, or video and sound alternating [`CHUNK_SECONDS`] each.
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
        let frame_rate = u64::from(track.stream.frame_rate);
        let video = runs(
            Kind::Video,
            track.samples.iter().map(|sample| {
                let window = (sample.index - first_index) / frame_rate / CHUNK_SECONDS;
                (window, u64::from(sample.size))
            }),
        );
        let rate = u64::from(sound.clip.sample_rate);
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

    fn bytes(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.bytes).sum()
    }
}

struct Placement {
    counts: Vec<u32>,
    offsets: Vec<u64>,
}

/// One chunk per window of the (window, bytes) samples, tagged with its window.
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

/// Merges by window, video first within a window.
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

/// Why a clip couldn't be written; a message ends with its cause, which `source()` doesn't
/// repeat.
#[derive(Debug, Error)]
pub(crate) enum MuxError {
    #[error("the replay buffer is empty")]
    Empty,
    #[error("the buffered video has no sequence header")]
    NoSequenceHeader,
    #[error("the clip is too large for an MP4 index")]
    TooLarge,
    #[error("the buffered video changed while it was being saved")]
    Changed,
    #[error("build the clip's index: {0}")]
    Index(mp4_atom::Error),
    #[error("write the clip: {0}")]
    Io(io::Error),
    #[error("read the buffer back: {0}")]
    Read(StoreError),
}

impl From<mp4_atom::Error> for MuxError {
    fn from(error: mp4_atom::Error) -> Self {
        Self::Index(error)
    }
}

impl From<io::Error> for MuxError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for MuxError {
    fn from(error: StoreError) -> Self {
        Self::Read(error)
    }
}

pub(crate) type Visit<'a> = dyn FnMut(u64, bool, &[u8]) -> Result<(), MuxError> + 'a;

/// A clip's packets, oldest first from a keyframe, visited twice: for the index, then the media.
pub(crate) trait Packets {
    fn visit(&self, each: &mut Visit<'_>) -> Result<(), MuxError>;
}

#[cfg(test)]
impl Packets for [Arc<Packet>] {
    fn visit(&self, each: &mut Visit<'_>) -> Result<(), MuxError> {
        self.iter()
            .try_for_each(|packet| each(packet.index, packet.keyframe, &packet.data))
    }
}

#[cfg(test)]
pub(crate) fn write(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
) -> Result<(), MuxError> {
    write_from(output, stream, packets, None)
}

#[cfg(test)]
pub(crate) fn write_with_audio(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &[Arc<Packet>],
    audio: &AudioClip,
) -> Result<(), MuxError> {
    write_from(output, stream, packets, Some(audio))
}

pub(crate) fn write_from<P: Packets + ?Sized>(
    output: &mut impl io::Write,
    stream: &Stream,
    packets: &P,
    audio: Option<&AudioClip>,
) -> Result<(), MuxError> {
    let (decoder, samples) = index(stream.codec, packets)?;
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
    write_media(output, &track, sound.as_ref(), &layout, packets)?;
    output.flush()?;
    Ok(())
}

/// First pass: the decoder record from the first packet carrying one, and a sample per packet.
fn index<P: Packets + ?Sized>(
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

/// A skipped frame index lengthens the previous sample.
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
        last.duration = duration_between(last.index, index)?;
    }
    samples.push(Sample {
        index,
        size,
        duration: TICKS_PER_FRAME,
        keyframe,
    });
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

fn duration_between(from: u64, to: u64) -> Result<u32, MuxError> {
    let frames: u32 = fit(to.saturating_sub(from).max(1))?;
    frames
        .checked_mul(TICKS_PER_FRAME)
        .ok_or(MuxError::TooLarge)
}

fn header(
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
) -> Result<Vec<u8>, MuxError> {
    let mut file = Vec::new();
    file_type(track.stream.codec).encode(&mut file)?;
    // Offsets are 64-bit whatever their value, so the index is the same length either way.
    let mut probe = Vec::new();
    movie(track, sound, layout, 0)?.encode(&mut probe)?;
    let media_offset = (file.len() + probe.len()) as u64 + MDAT_HEADER_BYTES;
    movie(track, sound, layout, media_offset)?.encode(&mut file)?;
    Ok(file)
}

fn write_media<P: Packets + ?Sized>(
    output: &mut impl io::Write,
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
    packets: &P,
) -> Result<(), MuxError> {
    output.write_all(&LARGE_SIZE.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(&(MDAT_HEADER_BYTES + layout.bytes()).to_be_bytes())?;
    let mut writer = MediaWriter {
        output,
        track,
        sound,
        chunks: layout.chunks.iter(),
        written: 0,
        chunk_end: 0,
    };
    packets.visit(&mut |_, _, data| writer.video_sample(data))?;
    writer.finish()
}

struct MediaWriter<'a, W> {
    output: &'a mut W,
    track: &'a Track<'a>,
    sound: Option<&'a Sound<'a>>,
    chunks: slice::Iter<'a, Chunk>,
    written: usize,
    chunk_end: usize,
}

impl<W: io::Write> MediaWriter<'_, W> {
    fn video_sample(&mut self, data: &[u8]) -> Result<(), MuxError> {
        let codec = self.track.stream.codec;
        let parts = sample_parts(codec, data);
        if parts.is_empty() {
            return Ok(());
        }
        let indexed = self
            .track
            .samples
            .get(self.written)
            .map(|sample| sample.size);
        if indexed != Some(stored_size(&parts, prefix_bytes(codec))?) {
            return Err(MuxError::Changed);
        }
        if self.written == self.chunk_end {
            self.next_video_chunk()?;
        }
        for part in parts {
            if length_prefixed(codec) {
                let length: u32 = fit(part.len())?;
                self.output.write_all(&length.to_be_bytes())?;
            }
            self.output.write_all(part)?;
        }
        self.written += 1;
        Ok(())
    }

    fn next_video_chunk(&mut self) -> Result<(), MuxError> {
        while let Some(chunk) = self.chunks.next() {
            match chunk.kind {
                Kind::Audio => self.sound_chunk(chunk)?,
                Kind::Video => {
                    self.chunk_end = chunk.first + chunk.count;
                    return Ok(());
                }
            }
        }
        Err(MuxError::Changed)
    }

    fn sound_chunk(&mut self, chunk: &Chunk) -> Result<(), MuxError> {
        if let Some(sound) = self.sound {
            for frame in &sound.clip.frames[chunk.first..chunk.first + chunk.count] {
                self.output.write_all(frame)?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), MuxError> {
        if self.written != self.track.samples.len() {
            return Err(MuxError::Changed);
        }
        while let Some(chunk) = self.chunks.next() {
            match chunk.kind {
                Kind::Audio => self.sound_chunk(chunk)?,
                Kind::Video => return Err(MuxError::Changed),
            }
        }
        Ok(())
    }
}

/// H.264 and HEVC samples are length-prefixed NAL units; AV1 OBUs carry their own sizes.
const fn length_prefixed(codec: Codec) -> bool {
    !matches!(codec, Codec::Av1)
}

fn fit<T: TryFrom<U>, U>(value: U) -> Result<T, MuxError> {
    T::try_from(value).map_err(|_| MuxError::TooLarge)
}

fn file_type(codec: Codec) -> Ftyp {
    let codec_brand = match codec {
        Codec::H264 => Some(b"avc1"),
        // No widely used HEVC brand; players check `isom`, `iso2` and `mp41`.
        Codec::Hevc => None,
        Codec::Av1 => Some(b"av01"),
    };
    Ftyp {
        major_brand: FourCC::new(b"isom"),
        minor_version: FILE_TYPE_MINOR_VERSION,
        compatible_brands: [Some(b"isom"), Some(b"iso2"), codec_brand, Some(b"mp41")]
            .into_iter()
            .flatten()
            .map(FourCC::new)
            .collect(),
    }
}

fn movie(
    track: &Track<'_>,
    sound: Option<&Sound<'_>>,
    layout: &Layout,
    media_offset: u64,
) -> Result<Moov, MuxError> {
    let durations = Durations::of(track)?;
    let mut tracks = vec![video_track(
        track,
        &durations,
        layout.placement(Kind::Video, media_offset)?,
    )?];
    if let Some(sound) = sound {
        tracks.push(sound_track(
            sound,
            durations.movie,
            layout.placement(Kind::Audio, media_offset)?,
        )?);
    }
    let next_track_id = fit::<u32, _>(tracks.len())? + 1;
    Ok(Moov {
        mvhd: Mvhd {
            timescale: MOVIE_TIMESCALE,
            duration: durations.movie.into(),
            rate: 1.into(),
            volume: 1.into(),
            next_track_id,
            ..Mvhd::default()
        },
        trak: tracks,
        ..Moov::default()
    })
}

fn video_track(
    track: &Track<'_>,
    durations: &Durations,
    placement: Placement,
) -> Result<Trak, MuxError> {
    let samples = track.samples;
    let keyframes = (1..)
        .zip(samples)
        .filter(|(_, sample)| sample.keyframe)
        .map(|(number, _)| number)
        .collect();
    Ok(Trak {
        tkhd: Tkhd {
            track_id: VIDEO_TRACK_ID,
            duration: durations.movie.into(),
            enabled: true,
            in_movie: true,
            width: fit::<u16, _>(track.stream.width)?.into(),
            height: fit::<u16, _>(track.stream.height)?.into(),
            ..Tkhd::default()
        },
        mdia: Mdia {
            mdhd: media_header(durations.timescale, durations.media),
            hdlr: handler(*b"vide", "VideoHandler"),
            minf: Minf {
                vmhd: Some(Vmhd::default()),
                dinf: data_information(),
                stbl: Stbl {
                    stsd: Stsd {
                        codecs: vec![visual_entry(track.stream, track.decoder)?],
                    },
                    stts: time_to_sample(samples.iter().map(|sample| sample.duration)),
                    stss: Some(Stss { entries: keyframes }),
                    stsc: sample_to_chunk(&placement.counts),
                    stsz: sample_sizes(samples.iter().map(|sample| sample.size).collect()),
                    co64: Some(Co64 {
                        entries: placement.offsets,
                    }),
                    ..Stbl::default()
                },
                ..Minf::default()
            },
        },
        ..Trak::default()
    })
}

fn media_header(timescale: u32, duration: u32) -> Mdhd {
    Mdhd {
        timescale,
        duration: duration.into(),
        language: UNDETERMINED_LANGUAGE.to_owned(),
        ..Mdhd::default()
    }
}

fn handler(kind: [u8; 4], name: &str) -> Hdlr {
    Hdlr {
        handler: FourCC::new(&kind),
        name: name.to_owned(),
    }
}

fn data_information() -> Dinf {
    Dinf {
        dref: Dref {
            urls: vec![Url::default()],
        },
    }
}

fn visual_entry(stream: &Stream, decoder: &DecoderConfig) -> Result<SampleEntry, MuxError> {
    let visual = Visual {
        data_reference_index: DATA_REFERENCE,
        width: fit(stream.width)?,
        height: fit(stream.height)?,
        ..Visual::default()
    };
    let colr = Some(Colr::Nclx {
        colour_primaries: BT709,
        transfer_characteristics: BT709,
        matrix_coefficients: stream.matrix.code().into(),
        full_range_flag: false,
    });
    Ok(match decoder {
        DecoderConfig::Avc(avcc) => Avc1 {
            visual,
            avcc: avcc.clone(),
            colr,
            ..Avc1::default()
        }
        .into(),
        // Parameter sets live only in the configuration record (`hvc1`, not `hev1`).
        DecoderConfig::Hevc(hvcc) => Hvc1 {
            visual,
            hvcc: hvcc.clone(),
            colr,
            ..Hvc1::default()
        }
        .into(),
        DecoderConfig::Av1(av1c) => Av01 {
            visual,
            av1c: av1c.clone(),
            colr,
            ..Av01::default()
        }
        .into(),
    })
}

fn time_to_sample(durations: impl IntoIterator<Item = u32>) -> Stts {
    let mut entries: Vec<SttsEntry> = Vec::new();
    for sample_delta in durations {
        match entries.last_mut() {
            Some(run) if run.sample_delta == sample_delta => run.sample_count += 1,
            _ => entries.push(SttsEntry {
                sample_count: 1,
                sample_delta,
            }),
        }
    }
    Stts { entries }
}

fn sample_to_chunk(counts: &[u32]) -> Stsc {
    let mut entries: Vec<StscEntry> = Vec::new();
    for (first_chunk, &samples_per_chunk) in (1..).zip(counts) {
        if entries
            .last()
            .is_none_or(|previous| previous.samples_per_chunk != samples_per_chunk)
        {
            entries.push(StscEntry {
                first_chunk,
                samples_per_chunk,
                sample_description_index: u32::from(DATA_REFERENCE),
            });
        }
    }
    Stsc { entries }
}

fn sample_sizes(sizes: Vec<u32>) -> Stsz {
    Stsz {
        samples: StszSamples::Different { sizes },
    }
}

/// The edit list skips the lead so the sound starts with the video; every AAC frame decodes on
/// its own, so there is no `stss`.
fn sound_track(sound: &Sound<'_>, duration: u32, placement: Placement) -> Result<Trak, MuxError> {
    let clip = sound.clip;
    Ok(Trak {
        tkhd: Tkhd {
            track_id: AUDIO_TRACK_ID,
            duration: duration.into(),
            enabled: true,
            in_movie: true,
            volume: 1.into(),
            ..Tkhd::default()
        },
        edts: Some(Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: duration.into(),
                    media_time: Some(sound.clip.lead.into()),
                    media_rate: 1.into(),
                }],
            }),
        }),
        mdia: Mdia {
            mdhd: media_header(clip.sample_rate, sound.media_duration()?),
            hdlr: handler(*b"soun", "SoundHandler"),
            minf: Minf {
                smhd: Some(Smhd::default()),
                dinf: data_information(),
                stbl: Stbl {
                    stsd: Stsd {
                        codecs: vec![audio_entry(sound)?],
                    },
                    stts: Stts {
                        entries: vec![SttsEntry {
                            sample_count: fit(sound.sizes.len())?,
                            sample_delta: FRAME_SAMPLES,
                        }],
                    },
                    stsc: sample_to_chunk(&placement.counts),
                    stsz: sample_sizes(sound.sizes.clone()),
                    co64: Some(Co64 {
                        entries: placement.offsets,
                    }),
                    ..Stbl::default()
                },
                ..Minf::default()
            },
        },
        ..Trak::default()
    })
}

fn audio_entry(sound: &Sound<'_>) -> Result<SampleEntry, MuxError> {
    let clip = sound.clip;
    let (average, peak) = sound.bitrates();
    let largest = sound.sizes.iter().copied().max().unwrap_or(0);
    Ok(Mp4a {
        audio: Audio {
            data_reference_index: DATA_REFERENCE,
            channel_count: clip.channels,
            sample_size: AUDIO_SAMPLE_BITS,
            // 16.16 fixed point, so it must fit 16 bits.
            sample_rate: fit::<u16, _>(clip.sample_rate)?.into(),
        },
        esds: Esds {
            es_desc: EsDescriptor {
                // Elementary streams in MP4 files have id 0.
                es_id: 0,
                dec_config: EsDecoderConfig {
                    object_type_indication: OBJECT_TYPE_MPEG4_AUDIO,
                    stream_type: STREAM_TYPE_AUDIO,
                    up_stream: 0,
                    buffer_size_db: fit(largest.min(u24::MAX))?,
                    max_bitrate: peak,
                    avg_bitrate: average,
                    dec_specific: Some(DecoderSpecific {
                        raw: clip.config.to_vec(),
                        ..DecoderSpecific::default()
                    }),
                },
                sl_config: SLConfig {},
            },
        },
        btrt: None,
        taic: None,
    }
    .into())
}

#[cfg(test)]
pub(super) mod tests;
