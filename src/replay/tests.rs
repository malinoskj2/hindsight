//! The replay buffer and saving, end to end: rings, stores, saves and their sound.

use super::*;
use std::assert_matches;
use store::Data;

const STREAM: Stream = Stream {
    codec: Codec::H264,
    width: 1280,
    height: 720,
    frame_rate: 30,
    matrix: Matrix::Bt709,
};

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "the stream was set twice")]
fn setting_the_stream_twice_is_a_bug() {
    let live = Live::new(10);
    live.set_stream(STREAM);
    live.set_stream(STREAM);
}

#[test]
fn a_save_error_names_its_step_and_chains_its_cause_once() {
    let error = SaveError::io("create", Path::new("clip.mp4"))(io::Error::other("denied"));
    assert_eq!(error.to_string(), "create clip.mp4");
    assert_eq!(log::chain(&error).to_string(), "create clip.mp4: denied");
    let error = SaveError::from(MuxError::from(io::Error::other("full")));
    assert_eq!(error.to_string(), "write the clip");
    assert_eq!(log::chain(&error).to_string(), "write the clip: full");
    let read = StoreError::call("read a page back")(io::Error::other("lost"));
    let error = SaveError::from(MuxError::from(read));
    assert_eq!(
        log::chain(&error).to_string(),
        "read the buffer back: read a page back: lost"
    );
}

/// The real encoder output in `tests/fixtures` is 128×72.
fn fixture_stream(codec: Codec) -> Stream {
    Stream {
        codec,
        width: 128,
        height: 72,
        ..STREAM
    }
}

/// An H.264 packet of at least `bytes`, a keyframe every 30 frames.
fn h264_packet(index: u64, bytes: usize) -> Packet {
    let keyframe = index.is_multiple_of(30);
    let mut data = if keyframe {
        h264::tests::keyframe()
    } else {
        h264::tests::inter_frame()
    };
    data.resize(data.len().max(bytes), 0xAB);
    Packet {
        index,
        keyframe,
        data: data.into(),
    }
}

fn push_frames(live: &Live, frames: u64) {
    for index in 0..frames {
        live.push(
            h264_packet(index, 0),
            if index % 2 == 0 {
                Picture::Fresh
            } else {
                Picture::Repeated
            },
        );
    }
}

fn live_with(frames: u64) -> Live {
    let live = Live::new(60);
    live.set_stream(STREAM);
    push_frames(&live, frames);
    live
}

#[test]
fn counts_frames_and_snapshots_the_buffer() {
    let live = live_with(100);

    assert_eq!(live.counters(), (100, 50));
    let Held { frames, bytes, .. } = live.held();
    assert_eq!(frames, 70);
    assert!(bytes > 0);

    let clip = live.clip().unwrap();
    assert_eq!(clip.packets.len(), 70);
    assert!(clip.packets[0].keyframe);
    assert!((clip.seconds() - 70.0 / 30.0).abs() < 1e-9);
}

#[test]
fn a_buffer_in_a_store_saves_the_same_clip() {
    let in_memory = live_with(100).clip().unwrap();
    let mut expected = Vec::new();
    mp4::write(&mut expected, &in_memory.stream, &in_memory.packets).unwrap();
    for (piece, fail_after) in [(1000, usize::MAX), (7, usize::MAX), (7, 50)] {
        let live = Live::new(60);
        live.set_stream(STREAM);
        let mut memory_store = store::tests::MemoryStore::new(1 << 16, piece);
        memory_store.fail_after = fail_after;
        live.set_store(Box::new(memory_store));
        push_frames(&live, 100);
        let snapshot = live.snapshot().unwrap();
        assert!((snapshot.seconds() - in_memory.seconds()).abs() < 1e-9);
        let kinds: Vec<bool> = snapshot
            .entries
            .iter()
            .map(|entry| matches!(entry.data, Data::Memory(_)))
            .collect();
        assert_eq!(kinds.contains(&true), fail_after != usize::MAX);
        assert!(!kinds[0], "the oldest packets are in the store");

        let directory = tempfile::tempdir().unwrap();
        let path = save(&snapshot, directory.path(), Container::Mp4)
            .unwrap()
            .path;
        assert_eq!(fs::read(path).unwrap(), expected, "pieces of {piece}");

        let from_store = snapshot.clip().unwrap();
        assert_eq!(from_store.packets.len(), in_memory.packets.len());
        for (stored, kept) in from_store.packets.iter().zip(&in_memory.packets) {
            assert_eq!(stored.index, kept.index);
            assert_eq!(stored.keyframe, kept.keyframe);
            assert_eq!(stored.data, kept.data);
        }
    }
}

#[test]
fn a_buffer_on_disk_saves_the_same_clip() {
    let push = |live: &Live| {
        for index in 0..100 {
            let bytes = 200 + 5 * usize::try_from(index).unwrap();
            live.push(h264_packet(index, bytes), Picture::Fresh);
        }
    };
    let in_memory = Live::new(60);
    in_memory.set_stream(STREAM);
    push(&in_memory);
    let in_memory = in_memory.clip().unwrap();
    let mut expected = Vec::new();
    mp4::write(&mut expected, &in_memory.stream, &in_memory.packets).unwrap();

    let folder = tempfile::tempdir().unwrap();
    let store = disk::DiskStore::small(folder.path(), 4);
    let control = store.control();
    control.hold(true);
    let live = Live::new(60);
    live.set_stream(STREAM);
    live.set_store(Box::new(store));
    push(&live);
    let snapshot = live.snapshot().unwrap();
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| matches!(entry.data, Data::Stored(_)))
    );
    assert!(control.queued() > 0);

    let clips = tempfile::tempdir().unwrap();
    let path = save(&snapshot, clips.path(), Container::Mp4).unwrap().path;
    assert_eq!(fs::read(&path).unwrap(), expected, "from staging");
    fs::remove_file(path).unwrap();

    control.hold(false);
    control.settle();
    let path = save(&snapshot, clips.path(), Container::Mp4).unwrap().path;
    assert_eq!(fs::read(path).unwrap(), expected, "from the file");
}

#[test]
fn a_save_from_a_store_holds_a_few_packets_not_the_clip() {
    const FRAMES: usize = 120;
    const FRAME_BYTES: usize = 256 << 10;
    let live = Live::new(FRAMES as u64);
    live.set_stream(STREAM);
    live.set_store(Box::new(store::tests::MemoryStore::new(4 << 20, 1 << 20)));
    for (index, extra) in (0..).zip(0..FRAMES) {
        live.push(h264_packet(index, FRAME_BYTES + extra), Picture::Fresh);
    }
    let snapshot = live.snapshot().unwrap();
    let clip_bytes = FRAMES * FRAME_BYTES;
    let directory = tempfile::tempdir().unwrap();

    let saving = heap::peak_during(|| {
        save(&snapshot, directory.path(), Container::Mp4).unwrap();
    });
    assert!(
        saving < WRITE_BUFFER_BYTES + 2 * FRAME_BYTES + (256 << 10),
        "a save held {saving} bytes at once"
    );
    let reading_all = heap::peak_during(|| drop(snapshot.clip().unwrap()));
    assert!(reading_all >= clip_bytes, "{reading_all} < {clip_bytes}");
}

#[test]
fn keyframes_are_recognised_for_every_codec() {
    assert!(is_keyframe(Codec::H264, &h264::tests::keyframe()));
    assert!(!is_keyframe(Codec::H264, &h264::tests::inter_frame()));
    assert!(is_keyframe(Codec::Hevc, &hevc::tests::keyframe()));
    assert!(!is_keyframe(Codec::Hevc, &hevc::tests::inter_frame()));
    assert!(is_keyframe(Codec::Av1, &av1::tests::keyframe()));
    assert!(!is_keyframe(Codec::Av1, &av1::tests::inter_frame()));
}

pub(in crate::replay) fn encoded_fixture(codec: Codec) -> Vec<Packet> {
    let units: Vec<&[u8]> = match codec {
        Codec::H264 => {
            annexb::access_units(include_bytes!("../../tests/fixtures/h264.264"), |unit| {
                h264::nal_type(unit) == h264::NAL_AUD
            })
        }
        Codec::Hevc => {
            annexb::access_units(include_bytes!("../../tests/fixtures/hevc.265"), |unit| {
                hevc::nal_type(unit) == hevc::NAL_AUD
            })
        }
        Codec::Av1 => av1::temporal_units(include_bytes!("../../tests/fixtures/av1.obu")),
    };
    units
        .into_iter()
        .enumerate()
        .map(|(index, data)| Packet {
            index: index as u64,
            keyframe: is_keyframe(codec, data),
            data: data.into(),
        })
        .collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

#[test]
fn real_encoder_output_muxes_for_every_codec() {
    for codec in Codec::ALL {
        let packets = encoded_fixture(codec);
        assert_eq!(packets.len(), 75, "{codec}");
        let keyframes: Vec<u64> = packets
            .iter()
            .filter(|packet| packet.keyframe)
            .map(|packet| packet.index)
            .collect();
        assert_eq!(keyframes, [0, 30, 60], "{codec}");

        let mut ring = Ring::new(40);
        for packet in packets {
            ring.push(packet.into()).unwrap();
        }
        let clip = Clip {
            stream: fixture_stream(codec),
            packets: store::fetch(&ring.snapshot()).unwrap(),
            audio: None,
        };
        assert_eq!(clip.packets.len(), 45, "{codec}");

        let mut file = Vec::new();
        mp4::write(&mut file, &clip.stream, &clip.packets).unwrap();
        let top = mp4::tests::atoms(&file);
        let kinds: Vec<_> = top.iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [b"ftyp", b"moov", b"mdat"], "{codec}");

        let (moov, media) = mp4::tests::index(&file);
        assert_eq!(
            (moov.mvhd.timescale, moov.mvhd.duration),
            (1000, 1500),
            "{codec}"
        );
        let [track] = &moov.trak[..] else {
            panic!("{codec}: expected one track");
        };
        assert_eq!(track.tkhd.track_id, 1, "{codec}");
        assert_eq!(
            (track.mdia.mdhd.timescale, track.mdia.mdhd.duration),
            (30_000, 45_000),
            "{codec}"
        );
        let table = &track.mdia.minf.stbl;
        assert_eq!(table.stss.as_ref().unwrap().entries, [1, 31], "{codec}");
        assert_eq!(table.stts.entries.len(), 1, "{codec}");

        let samples: Vec<_> = clip
            .packets
            .iter()
            .map(|packet| mp4::tests::stored(codec, packet))
            .collect();
        let sample_sizes: Vec<_> = samples
            .iter()
            .map(|sample| u32::try_from(sample.len()).unwrap())
            .collect();
        assert_eq!(mp4::tests::sizes(table), sample_sizes, "{codec}");
        let [(offset, bytes)] = &mp4::tests::chunks(table, &samples)[..] else {
            panic!("{codec}: expected one chunk");
        };
        assert_eq!(*offset, media, "{codec}");
        assert_eq!(&file[*offset..], &bytes[..], "{codec}");
        let expected_media = match codec {
            Codec::H264 => 0x4CC2_8686_4D7A_C7C8,
            Codec::Hevc => 0x1944_D923_E869_BE8C,
            Codec::Av1 => 0x6028_8448_497C_A88C,
        };
        assert_eq!(fnv1a(top[2].1), expected_media, "{codec}");

        let record = mp4::tests::decoder_record(track);
        assert_eq!(
            record,
            configuration(codec, &clip.packets[0].data)
                .unwrap()
                .record(),
            "{codec}"
        );
        let expected_record = match codec {
            Codec::H264 => 0x5B56_C447_D3ED_352B,
            Codec::Hevc => 0x716B_345E_8965_10BA,
            Codec::Av1 => 0xDAD6_9D69_8A69_A58C,
        };
        assert_eq!(fnv1a(&record), expected_record, "{codec}");
        if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
            let name = format!("{}.mp4", codec.name().replace('.', "").to_lowercase());
            fs::write(Path::new(&directory).join(name), &file).unwrap();
        }
    }
}

#[test]
fn real_encoder_output_muxes_with_sound() {
    let live = Live::new(40);
    live.set_stream(fixture_stream(Codec::H264));
    live.set_origin(0);
    let frames = audio::tests::encoded_fixture();
    assert_eq!(frames.len(), 119);
    live.set_audio(audio::tests::both(0, FRAME_SAMPLES));
    for packet in encoded_fixture(Codec::H264) {
        live.push(packet, Picture::Fresh);
    }
    live.push_audio(Source::System, 0, audio::tests::numbered(0, 120_000));

    let clip = live.clip().unwrap();
    assert_eq!(clip.packets.len(), 45);
    let pcm = clip.audio.as_ref().unwrap();
    assert_eq!(pcm.start, 46 * 1024);
    assert_eq!(pcm.frames, 119 - 46);
    assert_eq!(pcm.lead, 49_024 - 46 * 1024);
    let audio = &audio::tests::aac_clip(frames[46..].to_vec(), pcm.lead);

    let mut file = Vec::new();
    mp4::write_with_audio(
        &mut file,
        &clip.stream,
        &clip.packets,
        std::slice::from_ref(audio),
    )
    .unwrap();
    if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
        fs::write(Path::new(&directory).join("h264-aac.mp4"), &file).unwrap();
    }

    let (moov, media) = mp4::tests::index(&file);
    assert_eq!((moov.mvhd.duration, moov.mvhd.next_track_id), (1500, 3));
    let [video, sound] = &moov.trak[..] else {
        panic!("expected two tracks");
    };
    assert_eq!((video.tkhd.track_id, sound.tkhd.track_id), (1, 2));
    assert_eq!(
        (sound.mdia.mdhd.timescale, sound.mdia.mdhd.duration),
        (48_000, 73 * 1024)
    );
    let edits = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries;
    assert_eq!(edits.len(), 1);
    assert_eq!(
        (edits[0].segment_duration, edits[0].media_time),
        (1500, Some(1920))
    );
    let sound_table = &sound.mdia.minf.stbl;
    assert!(sound_table.stss.is_none());
    let frame_sizes: Vec<_> = audio
        .frames
        .iter()
        .map(|frame| u32::try_from(frame.len()).unwrap())
        .collect();
    assert_eq!(mp4::tests::sizes(sound_table), frame_sizes);

    let video_samples: Vec<_> = clip
        .packets
        .iter()
        .map(|packet| mp4::tests::stored(Codec::H264, packet))
        .collect();
    let sound_samples: Vec<_> = audio.frames.iter().map(|frame| frame.to_vec()).collect();
    let tagged = |table, samples, is_video| {
        mp4::tests::chunks(table, samples)
            .into_iter()
            .map(move |(offset, bytes)| (offset, bytes, is_video))
    };
    let mut all: Vec<_> = tagged(&video.mdia.minf.stbl, &video_samples, true)
        .chain(tagged(sound_table, &sound_samples, false))
        .collect();
    all.sort_by_key(|(offset, _, _)| *offset);
    let order: Vec<_> = all.iter().map(|(_, _, is_video)| *is_video).collect();
    assert_eq!(
        order,
        [true, false, true, false],
        "video, sound, video, sound"
    );
    let mut next = media;
    for (offset, bytes, _) in all {
        assert_eq!(offset, next);
        assert_eq!(&file[offset..offset + bytes.len()], &bytes[..]);
        next += bytes.len();
    }
    assert_eq!(next, file.len());
    assert_eq!(fnv1a(mp4::tests::atoms(&file)[2].1), 0x56DC_4C5D_809C_3A4E);
    assert_eq!(
        fnv1a(&mp4::tests::decoder_record(video)),
        0x5B56_C447_D3ED_352B
    );
}

#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_decodes_both_sound_tracks_with_every_codec_and_container() {
    let directory = tempfile::tempdir().unwrap();
    let system = audio::tests::aac_clip(audio::tests::encoded_fixture(), 1024);
    let microphone = AudioClip {
        source: Source::Microphone,
        ..audio::tests::aac_clip(audio::tests::encoded_fixture()[..100].to_vec(), 1024)
    };
    let tracks = [system, microphone];
    for codec in Codec::ALL {
        let packets: Vec<_> = encoded_fixture(codec).into_iter().map(Arc::new).collect();
        for container in [Container::Mp4, Container::Mkv] {
            let path = directory
                .path()
                .join(format!("{codec}.{}", container.extension()));
            let mut file = Vec::new();
            let stream = fixture_stream(codec);
            match container {
                Container::Mp4 => mp4::write_from(&mut file, &stream, &packets[..], &tracks),
                Container::Mkv => mkv::write_from(&mut file, &stream, &packets[..], &tracks),
            }
            .unwrap();
            fs::write(&path, file).unwrap();

            let output = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-map", "0", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(output.status.success(), "{codec} {container:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                "",
                "{codec} {container:?}"
            );

            let probe = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_entries"])
                .args([
                    "stream=codec_name:stream_disposition=default",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&path)
                .output()
                .unwrap();
            let found = String::from_utf8_lossy(&probe.stdout).into_owned();
            let video = match codec {
                Codec::H264 => "h264,1",
                Codec::Hevc => "hevc,1",
                Codec::Av1 => "av1,1",
            };
            assert_eq!(
                found.lines().collect::<Vec<_>>(),
                [video, "aac,1", "aac,0"],
                "{codec} {container:?}"
            );

            let system = crate::audio::testing::decode_mono(&path, 0);
            let microphone = crate::audio::testing::decode_mono(&path, 1);
            assert!(!system.is_empty(), "{codec} {container:?}");
            assert!(
                microphone.len() < system.len(),
                "{codec} {container:?}: the microphone's track is the shorter"
            );
        }
    }
}

#[test]
fn the_sound_is_trimmed_with_the_video_and_cut_to_it() {
    let live = Live::new(60);
    live.set_stream(STREAM);
    live.set_origin(0);
    live.set_audio(audio::tests::both(0, 0));
    let read: u64 = 2880;
    let mut next_sample: u64 = 0;
    for index in 0..150 {
        live.push(h264_packet(index, 0), Picture::Fresh);
        let now = tick_time(0, 30, index);
        while i64::try_from((next_sample + read) * 10_000_000 / 48_000).unwrap() <= now {
            live.push_audio(
                Source::System,
                next_sample,
                audio::tests::numbered(next_sample, read),
            );
            next_sample += read;
        }
    }

    let clip = live.clip().unwrap();
    assert_eq!(clip.packets[0].index, 90);
    let audio = clip.audio.unwrap();
    assert_eq!(audio.start, 139 * 1024);
    assert_eq!(audio.lead, 144_000 - 139 * 1024);
    assert_eq!(audio.frames, 235 - 139);
    let mut samples = [7; 4];
    audio.tracks[0].read(139 * 1024, &mut samples);
    assert_eq!(samples, [22_336, -22_336, 22_337, -22_337]);
    audio.tracks[0].read(239_000, &mut samples);
    assert_eq!(samples, [0; 4]);

    live.push_audio(
        Source::System,
        next_sample,
        audio::tests::numbered(next_sample, read),
    );
    assert_eq!(
        live.sound().ring(Source::System).unwrap().runs()[0].0,
        139 * 1024
    );
}

#[cfg(windows)]
fn push_sound(live: &Live, start: u64, end: u64, sample: impl Fn(u64) -> i16) {
    let mut at = start;
    while at < end {
        let next = (at + 2880).min(end);
        let data: Arc<[i16]> = (at..next)
            .flat_map(|number| {
                let value = sample(number);
                [value, value]
            })
            .collect();
        live.push_audio(Source::System, at, data);
        at = next;
    }
}

#[cfg(windows)]
fn live_with_a_click() -> Live {
    let live = live_with(90);
    live.set_origin(0);
    live.set_audio(audio::tests::both(CLOCK_RATE / 3, 0));
    let click = |number| if number == 80_000 { 30_000 } else { 0 };
    push_sound(&live, 0, 104_000, click);
    push_sound(&live, 116_000, 137_600, click);
    live
}

#[cfg(windows)]
fn snapshot_with_sound(live: &Live) -> Option<Snapshot> {
    let snapshot = live.snapshot().unwrap();
    match crate::audio::encode(snapshot.audio.as_ref().unwrap()) {
        Ok(_) => Some(snapshot),
        Err(error) => {
            eprintln!("skipped: {error}");
            None
        }
    }
}

#[test]
#[cfg(windows)]
fn a_saved_clip_encodes_its_sound_in_line_with_the_video() {
    let live = live_with_a_click();
    let Some(snapshot) = snapshot_with_sound(&live) else {
        return;
    };
    let pcm = snapshot.audio.as_ref().unwrap();
    assert_eq!(
        (pcm.start, pcm.frames, pcm.lead),
        (30 * 1024, 125 - 30, 32_000 - 30 * 1024)
    );

    let directory = tempfile::tempdir().unwrap();
    let file = fs::read(
        save(&snapshot, directory.path(), Container::Mp4)
            .unwrap()
            .path,
    )
    .unwrap();
    let (moov, _) = mp4::tests::index(&file);
    let [_, sound, microphone] = &moov.trak[..] else {
        panic!("expected three tracks");
    };
    assert_eq!(moov.mvhd.duration, 2000);
    assert_eq!(
        microphone.mdia.mdhd.duration, sound.mdia.mdhd.duration,
        "the silent microphone's track is as long as the system sound's"
    );
    assert_eq!(
        (sound.mdia.mdhd.timescale, sound.mdia.mdhd.duration),
        (48_000, 95 * 1024)
    );
    let edits = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries;
    assert_eq!(
        edits
            .iter()
            .map(|edit| (edit.segment_duration, edit.media_time))
            .collect::<Vec<_>>(),
        [(2000, Some(1280))]
    );
    let sizes = mp4::tests::sizes(&sound.mdia.minf.stbl);
    assert_eq!(sizes.len(), 95);
    let bytes: u32 = sizes.iter().sum();
    assert!((40_000..=56_000).contains(&bytes), "{bytes} bytes");
}

#[test]
#[cfg(windows)]
#[ignore = "needs ffmpeg"]
fn the_click_plays_in_time_with_the_video() {
    let live = live_with_a_click();
    let snapshot = snapshot_with_sound(&live).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let clip = save(&snapshot, directory.path(), Container::Mp4)
        .unwrap()
        .path;
    let decoded = crate::audio::testing::decode_mono(&clip, 0);
    let peak = crate::audio::testing::peak(&decoded);
    println!(
        "{} samples decoded for 2 s of video, the click at {peak}",
        decoded.len()
    );
    assert_eq!(decoded.len(), 96_000);
    assert!(peak.abs_diff(48_000) <= 2, "the click at {peak}");
    let microphone = crate::audio::testing::decode_mono(&clip, 1);
    assert_eq!(microphone.len(), 96_000);
    assert!(microphone.iter().all(|&sample| sample.unsigned_abs() <= 1));
}

#[test]
#[cfg(windows)]
#[ignore = "a timing, not a check"]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the signal stays within ±12 000, and sample numbers are far below 2^52"
)]
fn encodes_five_minutes_of_sound() {
    let live = Live::new(9000);
    live.set_stream(STREAM);
    push_frames(&live, 9000);
    live.set_origin(0);
    live.set_audio(audio::tests::both(0, 0));
    let mut seed: u32 = 1;
    let noise: Vec<i16> = (0..48_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            i16::try_from(seed >> 20).unwrap() - 2048
        })
        .collect();
    push_sound(&live, 0, 300 * 48_000, |number| {
        let time = number as f64 / 48_000.0;
        let chord = (time * 2764.6).sin() * 5000.0 + (time * 3487.2).sin() * 4000.0;
        chord as i16 + noise[usize::try_from(number % 48_000).unwrap()]
    });
    let snapshot = live.snapshot().unwrap();
    let pcm = snapshot.audio.as_ref().unwrap();
    let start = std::time::Instant::now();
    let tracks = crate::audio::encode(pcm).unwrap();
    let audio = &tracks[0];
    println!(
        "{} frames ({} bytes) of AAC from 300 s of sound in {:?}",
        audio.frames.len(),
        audio.frames.iter().map(|frame| frame.len()).sum::<usize>(),
        start.elapsed()
    );
}

#[test]
fn nothing_to_save_before_the_stream_starts() {
    let live = Live::new(10);
    assert!(live.clip().is_none());
    live.set_stream(STREAM);
    assert!(live.clip().is_none());
}

#[test]
fn saves_complete_files_under_unique_names() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = live_with(40).snapshot().unwrap();
    let target = directory.path().join("clips");

    let first = save(&snapshot, &target, Container::Mp4).unwrap().path;
    let second = save(&snapshot, &target, Container::Mp4).unwrap().path;

    assert_ne!(first, second);
    assert!(
        first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("hindsight_")
    );
    assert_eq!(first.extension().unwrap(), "mp4");
    assert_eq!(&fs::read(&first).unwrap()[4..8], b"ftyp");
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 2);
}

#[test]
fn a_name_another_file_holds_is_skipped_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let name = ClipName::parse("hindsight_2026-10-12_21-14-05.mp4").unwrap();
    let taken = directory
        .path()
        .join(name.file_name(Container::Mp4.extension()));
    fs::write(&taken, b"theirs").unwrap();
    let partial = directory.path().join("clip.partial");
    fs::write(&partial, b"ours").unwrap();

    let path = claim_name(directory.path(), name, Container::Mp4.extension(), &partial).unwrap();

    assert_eq!(
        path,
        directory.path().join("hindsight_2026-10-12_21-14-05-2.mp4")
    );
    assert_eq!(fs::read(&path).unwrap(), b"ours");
    assert_eq!(fs::read(&taken).unwrap(), b"theirs");
    assert!(!partial.exists());
}

#[test]
fn a_failed_save_leaves_no_partial_file() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = Snapshot {
        stream: STREAM,
        entries: vec![Arc::new(h264_packet(1, 0).into())],
        audio: None,
    };

    assert_matches!(
        save(&snapshot, directory.path(), Container::Mp4),
        Err(SaveError::Mux(MuxError::NoSequenceHeader))
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

/// Counts the heap a thread holds, for the tests that bound a save's peak memory.
///
/// The `#[global_allocator]` here serves the whole lib test binary, not just these tests: every
/// test in the crate allocates through `Counting`. It only forwards to the system allocator and
/// adds to two thread-locals, so the others see no difference. It cannot move to an integration
/// test under `tests/`, since what it measures is crate-private.
#[expect(unsafe_code, reason = "wraps the system allocator to count bytes")]
mod heap {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    thread_local! {
        static HELD: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    struct Counting;

    // SAFETY: every call goes to the system allocator unchanged; counting touches no memory.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's promises about `layout` are the system allocator's.
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                count(size(layout.size()));
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as for `alloc`.
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                count(size(layout.size()));
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: the caller allocated `pointer` with `layout` in the system allocator.
            unsafe { System.dealloc(pointer, layout) };
            count(-size(layout.size()));
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as for `dealloc`; the caller's promises about `new_size` pass through.
            let moved = unsafe { System.realloc(pointer, layout, new_size) };
            if !moved.is_null() {
                count(size(new_size) - size(layout.size()));
            }
            moved
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    fn size(bytes: usize) -> isize {
        isize::try_from(bytes).unwrap_or(isize::MAX)
    }

    fn count(bytes: isize) {
        let _ = HELD.try_with(|held| {
            let now = held.get() + bytes;
            held.set(now);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }

    pub(super) fn peak_during(work: impl FnOnce()) -> usize {
        let before = HELD.with(Cell::get);
        PEAK.with(|peak| peak.set(before));
        work();
        usize::try_from(PEAK.with(Cell::get) - before).unwrap_or(0)
    }
}
