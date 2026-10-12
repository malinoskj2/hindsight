//! A minimal MP4 writer for one video track in H.264, HEVC or AV1 and, when there is sound, one
//! AAC track. The index (`moov`) is written before the media data, so players can start a clip
//! without reading to its end; with every packet already in memory, the offsets are known up
//! front.
//!
//! Video alone sits in one chunk. With sound, the media data alternates a second of video with
//! the second of sound that plays alongside it, so a player reads both from one place in the
//! file; the sound's edit list skips the samples of its first frames that play before the video
//! starts, so the two start together.
//!
//! The atoms are encoded by the `mp4-atom` crate. The layout is this module's: which samples
//! make a chunk and where the chunks sit, the edit list, and the index before the media data.

use super::{AudioClip, FRAME_SAMPLES, Packet, Stream, configuration, sample_parts};
use crate::config::Codec;
use mp4_atom::{
    Audio, Av01, Av1c, Avc1, Avcc, Co64, Codec as SampleEntry, Colr, Decode, Dinf, Dref, Edts,
    Elst, ElstEntry, Encode, Esds, FourCC, Ftyp, Hdlr, Hvc1, Hvcc, Mdhd, Mdia, Minf, Moov, Mp4a,
    Mvhd, Smhd, Stbl, Stsc, StscEntry, Stsd, Stss, Stsz, StszSamples, Stts, SttsEntry, Tkhd, Trak,
    Url, Visual, Vmhd,
    esds::{DecoderConfig, DecoderSpecific, EsDescriptor, SLConfig},
    u24,
};
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
/// The object type of MPEG-4 audio (AAC) and the stream type of audio, as the `esds` decoder
/// configuration states them (ISO/IEC 14496-1).
const OBJECT_TYPE_MPEG4_AUDIO: u8 = 0x40;
const STREAM_TYPE_AUDIO: u8 = 0x05;
/// The bits of a sample that an `mp4a` entry declares; AAC decodes to 16-bit samples.
const AUDIO_SAMPLE_BITS: u16 = 16;
/// The bytes of the length in front of each H.264 or HEVC NAL unit in a sample, which the
/// `avcC` and `hvcC` records declare.
pub(super) const NAL_LENGTH_BYTES: u8 = 4;
/// An atom size of 1 means a 64-bit size follows the atom type.
const LARGE_SIZE: u32 = 1;
/// A 16-byte `mdat` header with a 64-bit size, so clips larger than 4 GB need no special case.
const MDAT_HEADER_BYTES: u64 = 16;
/// `ftyp` minor version, as other muxers write it next to the `isom` brand.
const FILE_TYPE_MINOR_VERSION: u32 = 0x200;
/// The language every media header declares.
const UNDETERMINED_LANGUAGE: &str = "und";
/// The index of the one data reference, which says the media is in this file.
const DATA_REFERENCE: u16 = 1;
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
    #[error("build the clip's index: {0}")]
    Index(#[from] mp4_atom::Error),
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
    let mut file = Vec::new();
    file_type(track.stream.codec).encode(&mut file)?;
    // Offsets are 64-bit whatever their value, so the index has the same length either way.
    let mut probe = Vec::new();
    movie(track, sound, layout, 0)?.encode(&mut probe)?;
    let media_offset = (file.len() + probe.len()) as u64 + MDAT_HEADER_BYTES;
    movie(track, sound, layout, media_offset)?.encode(&mut file)?;
    Ok(file)
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
fn file_type(codec: Codec) -> Ftyp {
    let codec_brand = match codec {
        Codec::H264 => Some(b"avc1"),
        // There is no widely used HEVC-specific brand; `isom`, `iso2` and `mp41` are what
        // players check.
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

/// The `moov` atom: the movie header, the video track and the sound track if there is one, with
/// the media data starting at `media_offset`.
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
    // The next free track id follows the sound track's when there is one.
    let next_track_id = if sound.is_some() {
        AUDIO_TRACK_ID
    } else {
        TRACK_ID
    } + 1;
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

/// The video's `trak` atom: its header with the picture's size, and its media: the timing, the
/// handler, the sample description, then where each sample sits in time and in the file.
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
            track_id: TRACK_ID,
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

/// The `mdhd` atom: the media's time scale and its duration in it.
fn media_header(timescale: u32, duration: u32) -> Mdhd {
    Mdhd {
        timescale,
        duration: duration.into(),
        language: UNDETERMINED_LANGUAGE.to_owned(),
        ..Mdhd::default()
    }
}

/// The `hdlr` atom: what kind of media the track holds (`vide` or `soun`), and its name.
fn handler(kind: [u8; 4], name: &str) -> Hdlr {
    Hdlr {
        handler: FourCC::from(kind),
        name: name.to_owned(),
    }
}

/// The `dinf` atom: one data reference, saying the media is in this file.
fn data_information() -> Dinf {
    Dinf {
        dref: Dref {
            urls: vec![Url::default()],
        },
    }
}

/// The visual sample entry (`avc1`, `hvc1` or `av01`): the picture's size, then the decoder
/// configuration record and the colour.
fn visual_entry(stream: &Stream, decoder: &[u8]) -> Result<SampleEntry, MuxError> {
    let visual = Visual {
        data_reference_index: DATA_REFERENCE,
        width: fit(stream.width)?,
        height: fit(stream.height)?,
        ..Visual::default()
    };
    let colr = Some(Colr::Nclx {
        colour_primaries: BT709,
        transfer_characteristics: BT709,
        matrix_coefficients: BT709,
        full_range_flag: false,
    });
    // The record arrives as the atom bytes `configuration` built, and the entry takes it as
    // the crate's own type, so it is decoded back here.
    let mut record = decoder;
    Ok(match stream.codec {
        Codec::H264 => Avc1 {
            visual,
            avcc: Avcc::decode(&mut record)?,
            colr,
            ..Avc1::default()
        }
        .into(),
        // Parameter sets live only in the configuration record (`hvc1`, not `hev1`).
        Codec::Hevc => Hvc1 {
            visual,
            hvcc: Hvcc::decode(&mut record)?,
            colr,
            ..Hvc1::default()
        }
        .into(),
        Codec::Av1 => Av01 {
            visual,
            av1c: Av1c::decode(&mut record)?,
            colr,
            ..Av01::default()
        }
        .into(),
    })
}

/// The `stts` atom: the sample durations, as runs of equal ones.
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

/// The `stsc` atom: how many samples each chunk holds, as one entry per run of chunks that
/// hold the same number (the first chunk of the run, its count, the one description). Video
/// alone is one entry for its one chunk.
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

/// The `stsz` atom: every sample's size, listed.
fn sample_sizes(sizes: Vec<u32>) -> Stsz {
    Stsz {
        samples: StszSamples::Different { sizes },
    }
}

/// The sound's `trak` atom: its header, the edit that starts it with the video and lasts
/// `duration` in the movie's time scale, and its media, whose chunks sit as `placement` says.
/// Every frame lasts [`FRAME_SAMPLES`], and every AAC frame decodes on its own, so there is no
/// `stss`.
fn sound_track(sound: &Sound<'_>, duration: u32, placement: Placement) -> Result<Trak, MuxError> {
    let format = &sound.clip.format;
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
            mdhd: media_header(format.sample_rate, sound.media_duration()?),
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

/// The `mp4a` sample entry: the channels, the sample size and rate, then the `esds` atom,
/// whose elementary stream descriptor says the stream is AAC, gives its buffer size and
/// bitrates, and carries the `AudioSpecificConfig`.
fn audio_entry(sound: &Sound<'_>) -> Result<SampleEntry, MuxError> {
    let format = &sound.clip.format;
    let (average, peak) = sound.bitrates();
    let largest = sound.sizes.iter().copied().max().unwrap_or(0);
    Ok(Mp4a {
        audio: Audio {
            data_reference_index: DATA_REFERENCE,
            channel_count: format.channels,
            sample_size: AUDIO_SAMPLE_BITS,
            // The rate is 16.16 fixed point, so it must fit 16 bits.
            sample_rate: fit::<u16, _>(format.sample_rate)?.into(),
        },
        esds: Esds {
            es_desc: EsDescriptor {
                // Elementary streams in MP4 files have id 0.
                es_id: 0,
                dec_config: DecoderConfig {
                    object_type_indication: OBJECT_TYPE_MPEG4_AUDIO,
                    stream_type: STREAM_TYPE_AUDIO,
                    up_stream: 0,
                    buffer_size_db: fit(largest.min(u24::MAX))?,
                    max_bitrate: peak,
                    avg_bitrate: average,
                    dec_specific: Some(DecoderSpecific {
                        raw: format.config.to_vec(),
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

/// `atom`, a decoder configuration record (`avcC`, `hvcC` or `av1C`), as the bytes a sample
/// entry carries, unless it cannot be encoded.
pub(super) fn record(atom: &impl Encode) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    atom.encode(&mut bytes).ok()?;
    Some(bytes)
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

    /// The file's index, decoded, and the offset of its media data.
    pub(in crate::replay) fn index(file: &[u8]) -> (Moov, usize) {
        let mut rest = file;
        Ftyp::decode(&mut rest).unwrap();
        let moov = Moov::decode(&mut rest).unwrap();
        let media = file.len() - rest.len() + usize::try_from(MDAT_HEADER_BYTES).unwrap();
        (moov, media)
    }

    /// The sizes a sample table lists.
    pub(in crate::replay) fn sizes(table: &Stbl) -> Vec<u32> {
        match &table.stsz.samples {
            StszSamples::Different { sizes } => sizes.clone(),
            StszSamples::Identical { count, size } => vec![*size; *count as usize],
        }
    }

    /// Each chunk of a sample table as (offset, bytes), the bytes being those of its run of
    /// `samples`, which must be the track's samples as the media data stores them.
    pub(in crate::replay) fn chunks(table: &Stbl, samples: &[Vec<u8>]) -> Vec<(usize, Vec<u8>)> {
        let mut next = 0;
        let found: Vec<_> = (1..)
            .zip(&table.co64.as_ref().unwrap().entries)
            .map(|(number, &offset)| {
                let run = table
                    .stsc
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.first_chunk <= number)
                    .unwrap();
                let count = run.samples_per_chunk as usize;
                let bytes = samples[next..next + count].concat();
                next += count;
                (usize::try_from(offset).unwrap(), bytes)
            })
            .collect();
        assert_eq!(next, samples.len(), "every sample is in a chunk");
        found
    }

    /// A packet's bytes as the media data stores them.
    pub(in crate::replay) fn stored(codec: Codec, packet: &Packet) -> Vec<u8> {
        let mut bytes = Vec::new();
        for part in sample_parts(codec, &packet.data) {
            if length_prefixed(codec) {
                bytes.extend_from_slice(&u32::try_from(part.len()).unwrap().to_be_bytes());
            }
            bytes.extend_from_slice(part);
        }
        bytes
    }

    /// The decoder configuration record of a track's one sample entry, as atom bytes again.
    pub(in crate::replay) fn decoder_record(track: &Trak) -> Vec<u8> {
        let record = match &track.mdia.minf.stbl.stsd.codecs[..] {
            [SampleEntry::Avc1(entry)] => record(&entry.avcc),
            [SampleEntry::Hvc1(entry)] => record(&entry.hvcc),
            [SampleEntry::Av01(entry)] => record(&entry.av1c),
            other => panic!("expected one video sample entry, found {other:?}"),
        };
        record.unwrap()
    }

    /// Checks that every chunk of `table` sits in `file` holding its `samples`, and returns the
    /// chunk offsets.
    fn check_chunks(file: &[u8], table: &Stbl, samples: &[Vec<u8>]) -> Vec<usize> {
        chunks(table, samples)
            .into_iter()
            .map(|(offset, bytes)| {
                assert_eq!(&file[offset..offset + bytes.len()], bytes);
                offset
            })
            .collect()
    }

    fn total_duration(stts: &Stts) -> u64 {
        stts.entries
            .iter()
            .map(|entry| u64::from(entry.sample_count) * u64::from(entry.sample_delta))
            .sum()
    }

    /// The sample-to-chunk entries as (first chunk, samples per chunk) pairs.
    fn chunk_runs(stsc: &Stsc) -> Vec<(u32, u32)> {
        stsc.entries
            .iter()
            .map(|entry| (entry.first_chunk, entry.samples_per_chunk))
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

        let (moov, media) = index(&file);
        let table = &moov.trak[0].mdia.minf.stbl;
        assert_eq!(sizes(table), [9, 8, 8, 9, 8, 8]);
        assert_eq!(table.stss.as_ref().unwrap().entries, [1, 4]);
        assert_eq!(
            table.stts.entries,
            [SttsEntry {
                sample_count: 6,
                sample_delta: 1000
            }]
        );
        assert_eq!(chunk_runs(&table.stsc), [(1, 6)]);

        let samples: Vec<_> = packets
            .iter()
            .map(|packet| stored(Codec::H264, packet))
            .collect();
        assert_eq!(check_chunks(&file, table, &samples), [media]);
        assert_eq!(&file[media..media + 4], &5_u32.to_be_bytes());
        assert_eq!(&file[media + 4..media + 9], &h264::tests::IDR);
        assert_eq!(&file[media + 13..media + 17], &h264::tests::SLICE);
        assert_eq!(top[2].1.len(), 9 * 2 + 8 * 4);
        assert_eq!(media + top[2].1.len(), file.len());
    }

    #[test]
    fn describes_the_stream_and_its_decoder_configuration() {
        let file = mux(Codec::H264, &[packet(Codec::H264, 0, true)]);
        let (moov, _) = index(&file);

        assert_eq!(
            (
                moov.mvhd.timescale,
                moov.mvhd.duration,
                moov.mvhd.next_track_id
            ),
            (1000, 16, 2)
        );
        let [track] = &moov.trak[..] else {
            panic!("expected one track");
        };
        assert_eq!(track.tkhd.track_id, 1);
        assert_eq!(
            (track.tkhd.width, track.tkhd.height),
            (1920.into(), 1080.into())
        );
        assert_eq!(track.mdia.hdlr.handler, FourCC::new(b"vide"));
        assert_eq!(
            (track.mdia.mdhd.timescale, track.mdia.mdhd.duration),
            (60_000, 1000)
        );

        let [SampleEntry::Avc1(entry)] = &track.mdia.minf.stbl.stsd.codecs[..] else {
            panic!("expected one avc1 entry");
        };
        assert_eq!((entry.visual.width, entry.visual.height), (1920, 1080));
        assert_eq!(
            entry.avcc.sequence_parameter_sets,
            [h264::tests::SPS.to_vec()]
        );
        assert_eq!(
            entry.avcc.picture_parameter_sets,
            [h264::tests::PPS.to_vec()]
        );
        assert_eq!(
            entry.colr,
            Some(Colr::Nclx {
                colour_primaries: 1,
                transfer_characteristics: 1,
                matrix_coefficients: 1,
                full_range_flag: false,
            })
        );
        // The record in the file is the one the parameter sets built, byte for byte.
        assert_eq!(
            decoder_record(track),
            h264::configuration(&h264::tests::keyframe()).unwrap()
        );
    }

    #[test]
    fn hevc_gets_an_hvc1_entry_and_length_prefixed_samples() {
        let packets = [packet(Codec::Hevc, 0, true), packet(Codec::Hevc, 1, false)];
        let file = mux(Codec::Hevc, &packets);

        let (moov, _) = index(&file);
        let table = &moov.trak[0].mdia.minf.stbl;
        let [SampleEntry::Hvc1(entry)] = &table.stsd.codecs[..] else {
            panic!("expected one hvc1 entry");
        };
        assert_eq!(entry.hvcc.arrays.len(), 3);
        assert_eq!(
            decoder_record(&moov.trak[0]),
            hevc::configuration(&hevc::tests::keyframe()).unwrap()
        );
        assert_eq!(sizes(table)[0] as usize, hevc::tests::IDR.len() + 4);
    }

    #[test]
    fn av1_gets_its_brand_an_av01_entry_and_whole_obus_as_samples() {
        let packets = [packet(Codec::Av1, 0, true), packet(Codec::Av1, 1, false)];
        let file = mux(Codec::Av1, &packets);

        let file_type = Ftyp::decode(&mut &file[..]).unwrap();
        assert!(file_type.compatible_brands.contains(&FourCC::new(b"av01")));
        let (moov, media) = index(&file);
        let table = &moov.trak[0].mdia.minf.stbl;
        let [SampleEntry::Av01(entry)] = &table.stsd.codecs[..] else {
            panic!("expected one av01 entry");
        };
        assert_eq!(
            entry.av1c.config_obus[0] >> 3,
            1,
            "the record repeats the sequence header"
        );
        assert_eq!(
            decoder_record(&moov.trak[0]),
            av1::configuration(&av1::tests::keyframe()).unwrap()
        );

        // AV1 samples are the OBUs as they are, minus the temporal delimiter.
        let kept: usize = av1::sample_obus(&packets[0].data)
            .iter()
            .map(|obu| obu.len())
            .sum();
        assert_eq!(sizes(table)[0] as usize, kept);
        assert_eq!(
            file[media] >> 3,
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

        let (moov, _) = index(&file);
        let durations: Vec<_> = moov.trak[0]
            .mdia
            .minf
            .stbl
            .stts
            .entries
            .iter()
            .map(|entry| (entry.sample_count, entry.sample_delta))
            .collect();
        assert_eq!(durations, [(1, 1000), (1, 3000), (1, 1000)]);
        assert_eq!((moov.mvhd.timescale, moov.mvhd.duration), (1000, 83));
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
        let (moov, media) = index(&file);
        assert_eq!(
            (
                moov.mvhd.timescale,
                moov.mvhd.duration,
                moov.mvhd.next_track_id
            ),
            (1000, 2000, 3)
        );
        let [video, sound] = &moov.trak[..] else {
            panic!("expected two tracks");
        };

        // The video is described as before, split into a chunk per second.
        let video_table = &video.mdia.minf.stbl;
        assert_eq!(sizes(video_table).len(), 120);
        assert_eq!(total_duration(&video_table.stts), 120 * 1000);
        assert_eq!(chunk_runs(&video_table.stsc), [(1, 60)]);

        // The sound: its handler, time scale, length, edit, frames and description.
        assert_eq!(sound.mdia.hdlr.handler, FourCC::new(b"soun"));
        assert_eq!(
            (sound.mdia.mdhd.timescale, sound.mdia.mdhd.duration),
            (48_000, 100 * 1024)
        );
        assert_eq!(
            (sound.tkhd.track_id, sound.tkhd.duration, sound.tkhd.volume),
            (2, 2000, 1.into())
        );
        let edits = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries;
        assert_eq!(
            edits,
            &[ElstEntry {
                segment_duration: 2000,
                media_time: Some(1500),
                media_rate: 1.into(),
            }]
        );
        let sound_table = &sound.mdia.minf.stbl;
        assert_eq!(
            sound_table.stts.entries,
            [SttsEntry {
                sample_count: 100,
                sample_delta: 1024
            }]
        );
        assert!(sound_table.stss.is_none());
        let frame_sizes: Vec<u32> = frames
            .iter()
            .map(|frame| u32::try_from(frame.len()).unwrap())
            .collect();
        assert_eq!(sizes(sound_table), frame_sizes);
        // Frames start each second of playback from the end of the lead: 49, 47 and 4 of them.
        assert_eq!(chunk_runs(&sound_table.stsc), [(1, 49), (2, 47), (3, 4)]);
        let [SampleEntry::Mp4a(entry)] = &sound_table.stsd.codecs[..] else {
            panic!("expected one mp4a entry");
        };
        assert_eq!(
            (
                entry.audio.channel_count,
                entry.audio.sample_size,
                entry.audio.sample_rate
            ),
            (2, 16, 48_000.into())
        );
        let config = &entry.esds.es_desc.dec_config;
        assert_eq!(
            (config.object_type_indication, config.stream_type),
            (0x40, 0x05)
        );
        assert_eq!(config.dec_specific.as_ref().unwrap().raw, [0x11, 0x90]);

        // The chunks alternate, a second of video then the sound that plays with it, and each
        // holds its samples' bytes.
        let video_samples: Vec<_> = packets
            .iter()
            .map(|packet| stored(Codec::H264, packet))
            .collect();
        let sound_samples: Vec<_> = frames.iter().map(|frame| frame.to_vec()).collect();
        let video_offsets = check_chunks(&file, video_table, &video_samples);
        let sound_offsets = check_chunks(&file, sound_table, &sound_samples);
        assert_eq!(video_offsets[0], media);
        assert!(video_offsets[0] < sound_offsets[0]);
        assert!(sound_offsets[0] < video_offsets[1]);
        assert!(video_offsets[1] < sound_offsets[1]);
        assert!(sound_offsets[1] < sound_offsets[2]);
        let media_end =
            sound_offsets[2] + frames[96..].iter().map(|frame| frame.len()).sum::<usize>();
        assert_eq!(media_end, file.len());
    }

    /// Times ten writes of a 300 s clip's worth of synthetic packets with sound, into memory:
    /// `cargo test --release -- --ignored --nocapture times_a_five_minute_clip`.
    #[test]
    #[ignore = "a timing, not a check"]
    fn times_a_five_minute_clip() {
        let packets: Vec<_> = (0..300 * 60)
            .map(|index| packet(Codec::H264, index, index % 60 == 0))
            .collect();
        let frames: Vec<Arc<[u8]>> = (0..300 * 48_000 / 1024)
            .map(|number: usize| Arc::from(vec![0_u8; 200 + number % 50]))
            .collect();
        let clip = AudioClip {
            format: crate::replay::audio::tests::format(1024),
            frames,
            lead: 0,
        };
        let mut file = Vec::new();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            file.clear();
            write_with_audio(&mut file, &stream(Codec::H264), &packets, &clip).unwrap();
        }
        println!(
            "ten writes of a 300 s clip with sound: {:?} for {} bytes each",
            start.elapsed(),
            file.len()
        );
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
