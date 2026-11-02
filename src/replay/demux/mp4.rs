//! The MP4 reader: the `moov` index, decoded with `mp4-atom`, and the sample tables it holds.

use super::{
    DemuxError, Place, Sound, Video, VideoSample, VideoTrack, finish_video, matrix_of,
    nearest_sample, track_kind,
};
use crate::{
    config::Codec,
    replay::{FRAME_SAMPLES, Matrix, mux::DecoderConfig, ticks_of},
};
use mp4_atom::{Codec as SampleEntry, Colr, Decode, FourCC, Stbl, StszSamples, Trak};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
};

const HEADER_BYTES: u64 = 8;
const LARGE_HEADER_BYTES: u64 = 16;
/// Size 1: a 64-bit size follows the type. Size 0: the atom runs to the end of the file.
const LARGE_SIZE: u32 = 1;
const TO_END: u32 = 0;
const OBJECT_TYPE_MPEG4_AUDIO: u8 = 0x40;
/// Larger than any index this program writes or a long recording needs.
const MAX_INDEX_BYTES: u64 = 256 << 20;

pub(super) fn read(file: &mut (impl Read + Seek)) -> Result<(Video, Vec<Sound>), DemuxError> {
    let moov = find_moov(file)?;
    let moov = mp4_atom::Moov::decode(&mut &moov[..])?;
    let movie_timescale = moov.mvhd.timescale.max(1);
    let (track, samples, frame_ticks) = moov
        .trak
        .iter()
        .find_map(|trak| video_track(trak, movie_timescale))
        .ok_or(DemuxError::NoVideo)?;
    let (video, first) = finish_video(track, samples, frame_ticks)?;
    let sounds = moov
        .trak
        .iter()
        .filter_map(|trak| sound(trak, movie_timescale, first))
        .collect();
    Ok((video, sounds))
}

/// Returns the `moov` atom, header included.
fn find_moov(file: &mut (impl Read + Seek)) -> Result<Vec<u8>, DemuxError> {
    let end = file.seek(SeekFrom::End(0))?;
    let mut at = 0;
    while at + HEADER_BYTES <= end {
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let size = match size {
            LARGE_SIZE => {
                let mut large = [0; 8];
                file.read_exact(&mut large)?;
                let size = u64::from_be_bytes(large);
                if size < LARGE_HEADER_BYTES {
                    return Err(DemuxError::Container);
                }
                size
            }
            TO_END => end - at,
            size if u64::from(size) < HEADER_BYTES => return Err(DemuxError::Container),
            size => u64::from(size),
        };
        if &header[4..] == b"moov" {
            if size > MAX_INDEX_BYTES || at + size > end {
                return Err(DemuxError::Container);
            }
            file.seek(SeekFrom::Start(at))?;
            let mut moov = vec![0; usize::try_from(size).map_err(|_| DemuxError::Container)?];
            file.read_exact(&mut moov)?;
            return Ok(moov);
        }
        at = at.checked_add(size).ok_or(DemuxError::Container)?;
    }
    Err(DemuxError::Container)
}

fn video_track(trak: &Trak, movie_timescale: u32) -> Option<(VideoTrack, Vec<VideoSample>, i64)> {
    if trak.mdia.hdlr.handler != FourCC::new(b"vide") {
        return None;
    }
    let stbl = &trak.mdia.minf.stbl;
    let (codec, config, visual, colr) = match stbl.stsd.codecs.first()? {
        SampleEntry::Avc1(entry) => (
            Codec::H264,
            DecoderConfig::Avc(entry.avcc.clone()),
            &entry.visual,
            &entry.colr,
        ),
        SampleEntry::Avc3(entry) => (
            Codec::H264,
            DecoderConfig::Avc(entry.avcc.clone()),
            &entry.visual,
            &entry.colr,
        ),
        SampleEntry::Hvc1(entry) => (
            Codec::Hevc,
            DecoderConfig::Hevc(entry.hvcc.clone()),
            &entry.visual,
            &entry.colr,
        ),
        SampleEntry::Hev1(entry) => (
            Codec::Hevc,
            DecoderConfig::Hevc(entry.hvcc.clone()),
            &entry.visual,
            &entry.colr,
        ),
        SampleEntry::Av01(entry) => (
            Codec::Av1,
            DecoderConfig::Av1(entry.av1c.clone()),
            &entry.visual,
            &entry.colr,
        ),
        _ => return None,
    };
    let matrix = match colr {
        Some(
            Colr::Nclx {
                matrix_coefficients,
                ..
            }
            | Colr::Nclc {
                matrix_coefficients,
                ..
            },
        ) => matrix_of(*matrix_coefficients),
        _ => Matrix::Bt709,
    };
    let timescale = trak.mdia.mdhd.timescale.max(1);
    let places = places(stbl)?;
    let decode_times = decode_times(stbl, places.len());
    let offset = edit_offset(trak, movie_timescale, timescale);
    let mut composition = composition_offsets(stbl);
    let sync: Option<Vec<u32>> = stbl.stss.as_ref().map(|stss| stss.entries.clone());
    let samples = places
        .into_iter()
        .zip(decode_times)
        .zip(1u32..)
        .map(|((place, decoded), number)| {
            let shown = decoded.saturating_add(composition.next().unwrap_or(0));
            VideoSample {
                place,
                time: offset.saturating_add(ticks_of(shown, timescale)),
                keyframe: sync
                    .as_ref()
                    .is_none_or(|sync| sync.binary_search(&number).is_ok()),
            }
        })
        .collect();
    let described = VideoTrack {
        codec,
        config,
        width: u32::from(visual.width),
        height: u32::from(visual.height),
        matrix,
    };
    Some((
        described,
        samples,
        ticks_of(usual_delta(stbl).max(1), timescale),
    ))
}

/// An AAC track of whole frames, with its start counted from `first`, the clock time of the
/// first picture shown. Other sound is left out.
fn sound(trak: &Trak, movie_timescale: u32, first: i64) -> Option<Sound> {
    if trak.mdia.hdlr.handler != FourCC::new(b"soun") {
        return None;
    }
    let stbl = &trak.mdia.minf.stbl;
    let SampleEntry::Mp4a(entry) = stbl.stsd.codecs.first()? else {
        return None;
    };
    let decoder = &entry.esds.es_desc.dec_config;
    if decoder.object_type_indication != OBJECT_TYPE_MPEG4_AUDIO {
        return None;
    }
    let config = decoder
        .dec_specific
        .as_ref()?
        .raw
        .clone()
        .into_boxed_slice();
    if stbl
        .stts
        .entries
        .iter()
        .any(|entry| entry.sample_delta != FRAME_SAMPLES)
    {
        return None;
    }
    let sample_rate = trak.mdia.mdhd.timescale;
    if sample_rate == 0 {
        return None;
    }
    let offset = edit_offset(trak, movie_timescale, sample_rate);
    Some(Sound {
        kind: track_kind(&trak.mdia.hdlr.name),
        sample_rate,
        channels: entry.audio.channel_count,
        config,
        frames: places(stbl)?,
        start: nearest_sample(offset.saturating_sub(first), sample_rate),
    })
}

/// Each sample's place, from its chunk's offset and the sizes of the samples before it there.
fn places(stbl: &Stbl) -> Option<Vec<Place>> {
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    };
    let chunks: Vec<u64> = match (&stbl.co64, &stbl.stco) {
        (Some(co64), _) => co64.entries.clone(),
        (None, Some(stco)) => stco.entries.iter().copied().map(u64::from).collect(),
        (None, None) => return None,
    };
    let mut places = Vec::with_capacity(sizes.len());
    let mut sizes = sizes.into_iter();
    let runs = &stbl.stsc.entries;
    for (number, &chunk) in (1u32..).zip(&chunks) {
        let per_chunk = runs
            .iter()
            .rev()
            .find(|run| run.first_chunk <= number)
            .map_or(0, |run| run.samples_per_chunk);
        let mut offset = chunk;
        for size in sizes.by_ref().take(per_chunk as usize) {
            places.push(Place { offset, size });
            offset += u64::from(size);
        }
    }
    Some(places)
}

fn decode_times(stbl: &Stbl, count: usize) -> Vec<i64> {
    let mut times = Vec::with_capacity(count);
    let mut time = 0i64;
    for entry in &stbl.stts.entries {
        for _ in 0..entry.sample_count {
            if times.len() == count {
                return times;
            }
            times.push(time);
            time = time.saturating_add(i64::from(entry.sample_delta));
        }
    }
    times.resize(count, time);
    times
}

fn composition_offsets(stbl: &Stbl) -> impl Iterator<Item = i64> + '_ {
    stbl.ctts.iter().flat_map(|ctts| {
        ctts.entries
            .iter()
            .flat_map(|entry| std::iter::repeat_n(entry.sample_offset, entry.sample_count as usize))
    })
}

/// The commonest sample duration, which is a frame's at the track's frame rate.
fn usual_delta(stbl: &Stbl) -> u32 {
    let mut counts: HashMap<u32, u64> = HashMap::new();
    for entry in &stbl.stts.entries {
        *counts.entry(entry.sample_delta).or_default() += u64::from(entry.sample_count);
    }
    counts
        .into_iter()
        .filter(|&(delta, _)| delta > 0)
        .max_by_key(|&(delta, count)| (count, std::cmp::Reverse(delta)))
        .map_or(1, |(delta, _)| delta)
}

/// The movie time, in clock ticks, at which the track's media time 0 plays: after the empty
/// edits at the start, less the media time the first edit skips.
fn edit_offset(trak: &Trak, movie_timescale: u32, media_timescale: u32) -> i64 {
    let Some(elst) = trak.edts.as_ref().and_then(|edts| edts.elst.as_ref()) else {
        return 0;
    };
    let mut empty = 0u64;
    for entry in &elst.entries {
        match entry.media_time {
            None => empty = empty.saturating_add(entry.segment_duration),
            Some(skipped) => {
                return ticks_of(empty, movie_timescale)
                    .saturating_sub(ticks_of(skipped, media_timescale));
            }
        }
    }
    ticks_of(empty, movie_timescale)
}
