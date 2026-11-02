//! The readers, checked against clips the writers made.

use super::*;
use crate::replay::{
    AudioClip, Packet,
    audio::tests::aac_clip,
    av1, h264, mkv, mp4,
    mp4::tests::stored,
    mux::tests::{packet, stream},
};
use std::{fs, io::Cursor, sync::Arc};

fn write(
    container: Container,
    codec: Codec,
    packets: &[Arc<Packet>],
    audio: &[AudioClip],
) -> Vec<u8> {
    let mut file = Vec::new();
    match container {
        Container::Mp4 => mp4::write_from(&mut file, &stream(codec), packets, audio),
        Container::Mkv => mkv::write_from(&mut file, &stream(codec), packets, audio),
    }
    .unwrap();
    file
}

fn read(container: Container, file: Vec<u8>) -> (Movie, Cursor<Vec<u8>>) {
    let mut cursor = Cursor::new(file);
    let movie = Movie::read(&mut cursor, container).unwrap();
    (movie, cursor)
}

/// Frames 0 to 5 and 8, with a keyframe every third, as a recording that skipped two ticks.
fn packets(codec: Codec) -> Vec<Arc<Packet>> {
    [0, 1, 2, 3, 4, 5, 8]
        .into_iter()
        .map(|index| packet(codec, index, index % 3 == 0))
        .collect()
}

fn frame_ticks(frames: i64) -> i64 {
    frames * CLOCK_RATE / 60
}

#[test]
fn an_mp4_gives_back_each_sample_its_time_and_its_keyframes() {
    for codec in Codec::ALL {
        let packets = packets(codec);
        let (movie, mut file) = read(Container::Mp4, write(Container::Mp4, codec, &packets, &[]));
        let video = &movie.video;
        assert_eq!(video.stream.codec, codec);
        assert_eq!(video.stream.frame_rate, 60);
        assert_eq!((video.stream.width, video.stream.height), (1920, 1080));
        assert_eq!(video.stream.matrix, Matrix::Bt709);
        let times: Vec<i64> = video.samples.iter().map(|sample| sample.time).collect();
        assert_eq!(times, [0, 1, 2, 3, 4, 5, 8].map(frame_ticks), "{codec:?}");
        let keyframes: Vec<bool> = video.samples.iter().map(|sample| sample.keyframe).collect();
        assert_eq!(keyframes, [true, false, false, true, false, false, false]);
        assert!(
            (video.duration - frame_ticks(9)).abs() <= 1,
            "{}",
            video.duration
        );
        for (sample, packet) in video.samples.iter().zip(&packets) {
            assert_eq!(
                read_place(&mut file, sample.place).unwrap(),
                stored(codec, packet)
            );
        }
        assert!(movie.sounds.is_empty());
    }
}

#[test]
fn an_mkv_gives_back_each_sample_its_time_to_the_millisecond() {
    for codec in Codec::ALL {
        let packets = packets(codec);
        let (movie, mut file) = read(Container::Mkv, write(Container::Mkv, codec, &packets, &[]));
        let video = &movie.video;
        assert_eq!(video.stream.codec, codec);
        assert_eq!(video.stream.frame_rate, 60);
        assert_eq!((video.stream.width, video.stream.height), (1920, 1080));
        let times: Vec<i64> = video
            .samples
            .iter()
            .map(|sample| sample.time / (CLOCK_RATE / 1000))
            .collect();
        assert_eq!(times, [0, 17, 33, 50, 67, 83, 133], "{codec:?}");
        let keyframes: Vec<bool> = video.samples.iter().map(|sample| sample.keyframe).collect();
        assert_eq!(keyframes, [true, false, false, true, false, false, false]);
        for (sample, packet) in video.samples.iter().zip(&packets) {
            assert_eq!(
                read_place(&mut file, sample.place).unwrap(),
                stored(codec, packet)
            );
        }
    }
}

#[test]
fn sound_starts_its_lead_before_the_first_picture_in_either_container() {
    let packets: Vec<_> = (0..120)
        .map(|index| packet(Codec::H264, index, index % 30 == 0))
        .collect();
    let frames: Vec<Arc<[u8]>> = (0..100_u8)
        .map(|number| Arc::from(vec![number; 10 + usize::from(number % 7)]))
        .collect();
    let mut microphone = aac_clip(frames[..90].to_vec(), 1200);
    microphone.kind = TrackKind::Microphone;
    microphone.channels = 1;
    let clips = [aac_clip(frames.clone(), 1500), microphone];
    for container in Container::ALL {
        let (movie, mut file) = read(container, write(container, Codec::H264, &packets, &clips));
        assert_eq!(movie.sounds.len(), 2, "{container:?}");
        for (sound, clip) in movie.sounds.iter().zip(&clips) {
            assert_eq!(sound.kind, clip.kind);
            assert_eq!(sound.sample_rate, 48_000);
            assert_eq!(sound.channels, clip.channels);
            assert_eq!(sound.config, clip.config);
            assert_eq!(sound.frames.len(), clip.frames.len());
            for (place, frame) in sound.frames.iter().zip(&clip.frames) {
                assert_eq!(read_place(&mut file, *place).unwrap(), &frame[..]);
            }
            let lead = -i64::from(clip.lead);
            // Matroska keeps times to the millisecond, 48 samples here.
            let tolerance = if container == Container::Mkv { 48 } else { 0 };
            assert!(
                (sound.start - lead).abs() <= tolerance,
                "{container:?} {:?} starts at {} for {lead}",
                clip.kind,
                sound.start
            );
        }
    }
}

#[test]
fn a_saved_cut_opens_again_from_its_own_file_beside_the_clips() {
    let directory = tempfile::tempdir().unwrap();
    let packets = packets(Codec::Hevc);
    let frames: Vec<Arc<[u8]>> = (0..20_u8)
        .map(|number| Arc::from(vec![number; 12]))
        .collect();
    for container in Container::ALL {
        let path = crate::replay::save_cut(
            directory.path(),
            container,
            &stream(Codec::Hevc),
            &packets,
            &[aac_clip(frames.clone(), 0)],
        )
        .unwrap();
        assert_eq!(path.parent(), Some(directory.path()));
        let (movie, mut file) = Movie::open(&path).unwrap();
        assert_eq!(movie.container, container);
        assert_eq!(movie.video.stream.codec, Codec::Hevc);
        assert_eq!(movie.video.samples.len(), packets.len());
        let first = movie.video.samples[0];
        assert_eq!(
            read_place(&mut file, first.place).unwrap(),
            stored(Codec::Hevc, &packets[0])
        );
        assert_eq!(movie.sounds.len(), 1);
        assert_eq!(movie.sounds[0].frames.len(), frames.len());
    }
    let names: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(
        names.iter().all(|name| !name.ends_with(".partial")),
        "{names:?}"
    );
}

#[test]
fn a_keyframe_carries_the_parameter_sets_in_annex_b() {
    let packets = packets(Codec::H264);
    let (movie, mut file) = read(
        Container::Mp4,
        write(Container::Mp4, Codec::H264, &packets, &[]),
    );
    let video = &movie.video;
    let mut input = |number: usize| {
        let sample = video.samples[number];
        video.decoder_input(
            &read_place(&mut file, sample.place).unwrap(),
            sample.keyframe,
        )
    };
    let units = |stream: &[u8]| -> Vec<Vec<u8>> {
        stream
            .split(|&byte| byte == 1)
            .skip(1)
            .map(|unit| unit.strip_suffix(&[0, 0, 0]).unwrap_or(unit).to_vec())
            .collect()
    };
    assert!(input(0).starts_with(&START_CODE));
    assert_eq!(
        units(&input(0)),
        [
            h264::tests::SPS.to_vec(),
            h264::tests::PPS.to_vec(),
            h264::tests::IDR.to_vec()
        ]
    );
    assert_eq!(units(&input(1)), [h264::tests::SLICE.to_vec()]);
}

#[test]
fn an_av1_temporal_unit_is_delimited_and_a_keyframe_repeats_the_sequence_header() {
    let packets = packets(Codec::Av1);
    let (movie, mut file) = read(
        Container::Mkv,
        write(Container::Mkv, Codec::Av1, &packets, &[]),
    );
    let video = &movie.video;
    let DecoderConfig::Av1(av1c) = &video.config else {
        panic!("an AV1 record");
    };
    let mut sample = |number: usize| read_place(&mut file, video.samples[number].place).unwrap();
    let key = sample(0);
    let input = video.decoder_input(&key, true);
    assert_eq!(
        input,
        [&TEMPORAL_DELIMITER[..], &av1c.config_obus, &key].concat()
    );
    let inter = sample(1);
    assert_eq!(
        video.decoder_input(&inter, false),
        [&TEMPORAL_DELIMITER[..], &inter].concat()
    );
    assert!(av1::is_keyframe(&key));
}

#[test]
fn decoding_starts_from_the_keyframe_at_or_before_the_time() {
    let packets = packets(Codec::Hevc);
    let (movie, _) = read(
        Container::Mp4,
        write(Container::Mp4, Codec::Hevc, &packets, &[]),
    );
    let video = &movie.video;
    assert_eq!(video.keyframe_before(0), 0);
    assert_eq!(video.keyframe_before(frame_ticks(2)), 0);
    assert_eq!(video.keyframe_before(frame_ticks(3)), 3);
    assert_eq!(video.keyframe_before(frame_ticks(100)), 3);
    assert_eq!(video.keyframe_before(-1), 0);
}

#[test]
fn length_prefixes_become_start_codes_and_an_overrun_ends_the_sample() {
    let mut output = Vec::new();
    annex_b(&mut output, &[0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC], 4);
    assert_eq!(output, [0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    let mut cut = Vec::new();
    annex_b(&mut cut, &[0, 2, 0xAA, 0xBB, 0, 9, 0xCC], 2);
    assert_eq!(cut, [0, 0, 0, 1, 0xAA, 0xBB]);
}

#[test]
fn other_files_are_refused() {
    for container in Container::ALL {
        let refused = Movie::read(&mut Cursor::new(b"not a clip at all".to_vec()), container);
        assert!(refused.is_err(), "{container:?}");
    }
}
