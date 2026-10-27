//! The MP4 layout, checked by reading the written atoms back.

use super::*;
use crate::replay::{Matrix, audio::TrackKind, av1, h264, hevc};
use std::assert_matches;

/// The atom's bytes, as the file holds them.
pub(in crate::replay) fn record(atom: &impl Encode) -> Vec<u8> {
    let mut bytes = Vec::new();
    atom.encode(&mut bytes).unwrap();
    bytes
}

impl DecoderConfig {
    pub(in crate::replay) fn record(&self) -> Vec<u8> {
        match self {
            Self::Avc(avcc) => record(avcc),
            Self::Hevc(hvcc) => record(hvcc),
            Self::Av1(av1c) => record(av1c),
        }
    }
}

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

pub(in crate::replay) fn index(file: &[u8]) -> (Moov, usize) {
    let mut rest = file;
    Ftyp::decode(&mut rest).unwrap();
    let moov = Moov::decode(&mut rest).unwrap();
    let media = file.len() - rest.len() + usize::try_from(MDAT_HEADER_BYTES).unwrap();
    (moov, media)
}

pub(in crate::replay) fn sizes(table: &Stbl) -> Vec<u32> {
    match &table.stsz.samples {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    }
}

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

pub(in crate::replay) fn decoder_record(track: &Trak) -> Vec<u8> {
    match &track.mdia.minf.stbl.stsd.codecs[..] {
        [SampleEntry::Avc1(entry)] => record(&entry.avcc),
        [SampleEntry::Hvc1(entry)] => record(&entry.hvcc),
        [SampleEntry::Av01(entry)] => record(&entry.av1c),
        other => panic!("expected one video sample entry, found {other:?}"),
    }
}

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

fn chunk_runs(stsc: &Stsc) -> Vec<(u32, u32)> {
    stsc.entries
        .iter()
        .map(|entry| (entry.first_chunk, entry.samples_per_chunk))
        .collect()
}

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
    assert_eq!(
        decoder_record(track),
        record(&h264::configuration(&h264::tests::keyframe()).unwrap())
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
        record(&hevc::configuration(&hevc::tests::keyframe()).unwrap())
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
        record(&av1::configuration(&av1::tests::keyframe()).unwrap())
    );

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
    let packets: Vec<_> = (0..120)
        .map(|index| packet(Codec::H264, index, index % 30 == 0))
        .collect();
    let frames: Vec<Arc<[u8]>> = (0..100_u8)
        .map(|number| Arc::from(vec![number; 10 + usize::from(number % 7)]))
        .collect();
    let clip = crate::replay::audio::tests::aac_clip(frames.clone(), 1500);
    let mut file = Vec::new();
    write_with_audio(&mut file, &stream(Codec::H264), &packets, &[clip]).unwrap();

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

    let video_table = &video.mdia.minf.stbl;
    assert_eq!(sizes(video_table).len(), 120);
    assert_eq!(total_duration(&video_table.stts), 120 * 1000);
    assert_eq!(chunk_runs(&video_table.stsc), [(1, 60)]);

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
    let media_end = sound_offsets[2] + frames[96..].iter().map(|frame| frame.len()).sum::<usize>();
    assert_eq!(media_end, file.len());
}

#[test]
fn the_mix_is_the_enabled_sound_track_and_the_microphone_a_second_one() {
    let packets: Vec<_> = (0..120)
        .map(|index| packet(Codec::H264, index, index % 30 == 0))
        .collect();
    let frames = |fill: u8| -> Vec<Vec<u8>> {
        (0..100_u8)
            .map(|number| vec![fill.wrapping_add(number); 10 + usize::from(number % 5)])
            .collect()
    };
    let (mixed_frames, voice_frames) = (frames(0), frames(128));
    let clip = |frames: &[Vec<u8>]| {
        let frames = frames.iter().map(|frame| Arc::from(&frame[..])).collect();
        crate::replay::audio::tests::aac_clip(frames, 1500)
    };
    let mixed = AudioClip {
        kind: TrackKind::Mixed,
        ..clip(&mixed_frames)
    };
    let microphone = AudioClip {
        kind: TrackKind::Microphone,
        channels: 1,
        ..clip(&voice_frames)
    };
    let mut file = Vec::new();
    let tracks = [mixed, microphone];
    write_with_audio(&mut file, &stream(Codec::H264), &packets, &tracks).unwrap();

    let (moov, media) = index(&file);
    assert_eq!(moov.mvhd.next_track_id, 4);
    let [video, mixed, microphone] = &moov.trak[..] else {
        panic!("expected three tracks");
    };
    let header = |track: &Trak| {
        (
            track.tkhd.track_id,
            track.tkhd.alternate_group,
            track.tkhd.enabled,
            track.mdia.hdlr.name.clone(),
        )
    };
    assert_eq!(header(video), (1, 0, true, "VideoHandler".to_owned()));
    assert_eq!(
        header(mixed),
        (2, 1, true, "System sound and microphone".to_owned())
    );
    assert_eq!(header(microphone), (3, 1, false, "Microphone".to_owned()));
    assert_eq!(microphone.edts, mixed.edts);
    let [SampleEntry::Mp4a(entry)] = &microphone.mdia.minf.stbl.stsd.codecs[..] else {
        panic!("expected one mp4a entry");
    };
    assert_eq!(entry.audio.channel_count, 1);

    let video_samples: Vec<_> = packets
        .iter()
        .map(|packet| stored(Codec::H264, packet))
        .collect();
    let mut chunks: Vec<(usize, &str)> = [
        (video, &video_samples, "video"),
        (mixed, &mixed_frames, "mixed"),
        (microphone, &voice_frames, "microphone"),
    ]
    .into_iter()
    .flat_map(|(track, samples, name)| {
        check_chunks(&file, &track.mdia.minf.stbl, samples)
            .into_iter()
            .map(move |offset| (offset, name))
    })
    .collect();
    chunks.sort_unstable();
    assert_eq!(chunks[0].0, media);
    let order: Vec<_> = chunks.iter().map(|(_, name)| *name).collect();
    assert_eq!(
        order,
        [
            "video",
            "mixed",
            "microphone",
            "video",
            "mixed",
            "microphone",
            "mixed",
            "microphone"
        ]
    );
}

#[test]
#[ignore = "a timing, not a check"]
fn times_a_five_minute_clip() {
    let packets: Vec<_> = (0..300 * 60)
        .map(|index| packet(Codec::H264, index, index % 60 == 0))
        .collect();
    let frames: Vec<Arc<[u8]>> = (0..300 * 48_000 / 1024)
        .map(|number: usize| Arc::from(vec![0_u8; 200 + number % 50]))
        .collect();
    let clip = crate::replay::audio::tests::aac_clip(frames, 0);
    let mut file = Vec::new();
    let start = std::time::Instant::now();
    for _ in 0..10 {
        file.clear();
        write_with_audio(
            &mut file,
            &stream(Codec::H264),
            &packets,
            std::slice::from_ref(&clip),
        )
        .unwrap();
    }
    println!(
        "ten writes of a 300 s clip with sound: {:?} for {} bytes each",
        start.elapsed(),
        file.len()
    );
}

#[test]
fn refuses_empty_buffers_and_streams_without_a_sequence_header() {
    let mut file = Vec::new();
    for codec in Codec::ALL {
        assert_matches!(
            write(&mut file, &stream(codec), &[packet(codec, 0, false)]),
            Err(MuxError::NoSequenceHeader)
        );
    }
    assert_matches!(
        write(&mut file, &stream(Codec::H264), &[]),
        Err(MuxError::Empty)
    );
    assert_eq!(file.len(), 0);
}
