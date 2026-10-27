//! The Matroska layout, checked by reading the written elements back.

use super::*;
use crate::replay::{
    Packet,
    audio::{TrackKind, tests::aac_clip},
    mp4::tests::{index as mp4_index, packet, stream},
};
use std::{assert_matches, sync::Arc};

/// An element read back: its ID, where it starts within its parent's body, and its body.
struct Read<'a> {
    id: u32,
    at: usize,
    body: &'a [u8],
}

fn vint(bytes: &[u8], keep_marker: bool) -> (u64, usize) {
    let length = bytes[0].leading_zeros() as usize + 1;
    let mut value = 0u64;
    for &byte in &bytes[..length] {
        value = value << 8 | u64::from(byte);
    }
    if !keep_marker {
        value &= (1 << (7 * length)) - 1;
    }
    (value, length)
}

fn children(bytes: &[u8]) -> Vec<Read<'_>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (id, id_length) = vint(&bytes[at..], true);
        let (size, size_length) = vint(&bytes[at + id_length..], false);
        let start = at + id_length + size_length;
        let end = start + usize::try_from(size).unwrap();
        assert!(end <= bytes.len(), "element {id:#x} runs past its parent");
        found.push(Read {
            id: u32::try_from(id).unwrap(),
            at,
            body: &bytes[start..end],
        });
        at = end;
    }
    found
}

fn child(bytes: &[u8], id: u32) -> &[u8] {
    children(bytes)
        .into_iter()
        .find(|element| element.id == id)
        .unwrap_or_else(|| panic!("no element {id:#x}"))
        .body
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, &byte| value << 8 | u64::from(byte))
}

fn segment(file: &[u8]) -> &[u8] {
    let top = children(file);
    assert_eq!(
        top.iter().map(|element| element.id).collect::<Vec<_>>(),
        [EBML, SEGMENT]
    );
    assert_eq!(
        std::str::from_utf8(child(top[0].body, DOC_TYPE)),
        Ok("matroska")
    );
    top[1].body
}

fn mkv(codec: Codec, packets: &[Arc<Packet>], audio: &[AudioClip]) -> Vec<u8> {
    let mut file = Vec::new();
    write_from(&mut file, &stream(codec), packets, audio).unwrap();
    file
}

/// Each block's track, time within the file and payload, in file order.
fn blocks(segment: &[u8]) -> Vec<(u8, u64, bool, &[u8])> {
    let mut found = Vec::new();
    for cluster in children(segment)
        .into_iter()
        .filter(|element| element.id == CLUSTER)
    {
        let time = number(child(cluster.body, TIMESTAMP));
        for block in children(cluster.body)
            .into_iter()
            .filter(|element| element.id == SIMPLE_BLOCK)
        {
            let offset = i16::from_be_bytes([block.body[1], block.body[2]]);
            found.push((
                block.body[0] & 0x7F,
                time.checked_add_signed(i64::from(offset)).unwrap(),
                block.body[3] & KEYFRAME_FLAG != 0,
                &block.body[4..],
            ));
        }
    }
    found
}

#[test]
fn the_seek_head_and_cues_point_at_their_elements() {
    let packets: Vec<_> = (0..9)
        .map(|index| packet(Codec::H264, index, index % 3 == 0))
        .collect();
    let file = mkv(Codec::H264, &packets, &[]);
    let segment = segment(&file);
    let elements = children(segment);
    assert_eq!(
        elements
            .iter()
            .map(|element| element.id)
            .collect::<Vec<_>>(),
        [SEEK_HEAD, INFO, TRACKS, CLUSTER, CLUSTER, CLUSTER, CUES]
    );

    for seek in children(child(segment, SEEK_HEAD)) {
        let target = u32::try_from(number(child(seek.body, SEEK_ID))).unwrap();
        let at = usize::try_from(number(child(seek.body, SEEK_POSITION))).unwrap();
        let pointed = elements.iter().find(|element| element.at == at).unwrap();
        assert_eq!(pointed.id, target);
    }

    let cues = children(child(segment, CUES));
    assert_eq!(cues.len(), 3);
    for (cue, frame) in cues.iter().zip([0_u64, 3, 6]) {
        let time = number(child(cue.body, CUE_TIME));
        assert_eq!(time, ticks(frame, 60));
        let positions = child(cue.body, CUE_TRACK_POSITIONS);
        let at = usize::try_from(number(child(positions, CUE_CLUSTER_POSITION))).unwrap();
        let cluster = elements.iter().find(|element| element.at == at).unwrap();
        assert_eq!(cluster.id, CLUSTER);
        assert_eq!(number(child(cluster.body, TIMESTAMP)), time);
    }
}

#[test]
fn blocks_hold_the_same_samples_as_the_mp4() {
    for codec in Codec::ALL {
        let packets: Vec<_> = (0..6)
            .map(|index| packet(codec, index, index % 3 == 0))
            .collect();
        let file = mkv(codec, &packets, &[]);
        let mut mp4 = Vec::new();
        mp4::write_from(&mut mp4, &stream(codec), &packets[..], &[]).unwrap();
        let (_, media) = mp4_index(&mp4);

        let found = blocks(segment(&file));
        let payload: Vec<u8> = found.iter().flat_map(|block| block.3.to_vec()).collect();
        assert_eq!(payload, mp4[media..], "{codec}");
        assert_eq!(
            found
                .iter()
                .map(|&(track, time, keyframe, _)| (track, time, keyframe))
                .collect::<Vec<_>>(),
            (0..6_u64)
                .map(|frame| (VIDEO_TRACK, ticks(frame, 60), frame % 3 == 0))
                .collect::<Vec<_>>(),
            "{codec}"
        );
    }
}

#[test]
fn the_track_carries_the_codec_and_its_decoder_record() {
    for (codec, id) in [
        (Codec::H264, "V_MPEG4/ISO/AVC"),
        (Codec::Hevc, "V_MPEGH/ISO/HEVC"),
        (Codec::Av1, "V_AV1"),
    ] {
        let packets = [packet(codec, 0, true)];
        let file = mkv(codec, &packets, &[]);
        let track = child(child(segment(&file), TRACKS), TRACK_ENTRY);
        assert_eq!(child(track, CODEC_ID), id.as_bytes());
        let record = crate::replay::configuration(codec, &packets[0].data)
            .unwrap()
            .record();
        assert_eq!(child(track, CODEC_PRIVATE), &record[8..], "{codec}");
        let video = child(track, VIDEO);
        assert_eq!(
            (
                number(child(video, PIXEL_WIDTH)),
                number(child(video, PIXEL_HEIGHT))
            ),
            (1920, 1080)
        );
        assert_eq!(number(child(track, DEFAULT_DURATION)), 16_666_666);
    }
}

#[test]
fn sound_is_interleaved_by_time_and_the_video_waits_out_its_lead() {
    let packets: Vec<_> = (0..120)
        .map(|index| packet(Codec::H264, index, index % 30 == 0))
        .collect();
    let frames: Vec<Arc<[u8]>> = (0..100_u8)
        .map(|number| Arc::from(vec![number; 10 + usize::from(number % 7)]))
        .collect();
    let clip = aac_clip(frames.clone(), 1500);
    let file = mkv(Codec::H264, &packets, std::slice::from_ref(&clip));
    let segment = segment(&file);

    let tracks = children(child(segment, TRACKS));
    let sound = tracks[1].body;
    assert_eq!(child(sound, CODEC_ID), b"A_AAC");
    assert_eq!(child(sound, CODEC_PRIVATE), &clip.config[..]);
    assert_eq!(
        f64::from_be_bytes(
            child(child(sound, AUDIO), SAMPLING_FREQUENCY)
                .try_into()
                .unwrap()
        ),
        48_000.0
    );

    let found = blocks(segment);
    let times: Vec<u64> = found.iter().map(|block| block.1).collect();
    assert!(times.is_sorted(), "blocks out of time order");
    let audio: Vec<_> = found
        .iter()
        .filter(|block| block.0 == FIRST_AUDIO_TRACK)
        .collect();
    assert_eq!(audio.len(), 100);
    assert!(
        audio
            .iter()
            .zip(&frames)
            .all(|(block, frame)| block.3 == &frame[..])
    );
    let first_video = found.iter().find(|block| block.0 == VIDEO_TRACK).unwrap();
    assert_eq!(first_video.1, ticks(1500, 48_000));

    let duration = child(child(segment, INFO), DURATION);
    assert_eq!(
        f64::from_be_bytes(duration.try_into().unwrap()),
        (ticks(1500, 48_000) + 2000) as f64
    );
}

#[test]
fn the_microphone_gets_a_second_sound_track_with_its_own_lead() {
    let packets: Vec<_> = (0..120)
        .map(|index| packet(Codec::H264, index, index % 30 == 0))
        .collect();
    let mixed = AudioClip {
        kind: TrackKind::Mixed,
        ..aac_clip(vec![Arc::from([1_u8; 12]); 100], 1500)
    };
    let microphone = AudioClip {
        kind: TrackKind::Microphone,
        channels: 1,
        ..aac_clip(vec![Arc::from([2_u8; 9]); 100], 500)
    };
    let file = mkv(Codec::H264, &packets, &[mixed, microphone]);
    let segment = segment(&file);

    let entries = children(child(segment, TRACKS));
    let described: Vec<_> = entries[1..]
        .iter()
        .map(|entry| {
            (
                number(child(entry.body, TRACK_NUMBER)),
                number(child(entry.body, FLAG_DEFAULT)),
                std::str::from_utf8(child(entry.body, NAME)).unwrap(),
                number(child(child(entry.body, AUDIO), CHANNELS)),
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            (2, 1, "System sound and microphone", 2),
            (3, 0, "Microphone", 1)
        ]
    );

    let found = blocks(segment);
    assert!(found.iter().map(|block| block.1).is_sorted());
    let first = |track: u8| found.iter().find(|block| block.0 == track).unwrap();
    let (system_lead, microphone_lead) = (ticks(1500, 48_000), ticks(500, 48_000));
    assert_eq!(first(VIDEO_TRACK).1, system_lead);
    assert_eq!(first(FIRST_AUDIO_TRACK).1, 0);
    assert_eq!(
        first(FIRST_AUDIO_TRACK + 1).1,
        system_lead - microphone_lead
    );
    for (track, payload) in [(FIRST_AUDIO_TRACK, [1_u8; 12].as_slice()), (3, &[2; 9])] {
        let own: Vec<_> = found.iter().filter(|block| block.0 == track).collect();
        assert_eq!(own.len(), 100);
        assert!(own.iter().all(|block| block.2 && block.3 == payload));
    }
}

#[test]
fn a_long_stretch_without_keyframes_still_starts_new_clusters() {
    let packets: Vec<_> = (0..700)
        .map(|index| packet(Codec::H264, index, index == 0))
        .collect();
    let file = mkv(Codec::H264, &packets, &[]);
    let clusters = children(segment(&file))
        .into_iter()
        .filter(|element| element.id == CLUSTER)
        .count();
    assert_eq!(clusters, 3);
}

#[test]
fn refuses_empty_buffers() {
    let mut file = Vec::new();
    assert_matches!(
        write_from(
            &mut file,
            &stream(Codec::H264),
            &[][..] as &[Arc<Packet>],
            &[]
        ),
        Err(MuxError::Empty)
    );
}

#[test]
fn sizes_take_the_fewest_bytes_and_ids_keep_their_marker() {
    assert_eq!(size(0), [0x80]);
    assert_eq!(size(126), [0xFE]);
    assert_eq!(size(127), [0x40, 0x7F]);
    assert_eq!(wide_size(5), [0x01, 0, 0, 0, 0, 0, 0, 5]);
    assert_eq!(id(SIMPLE_BLOCK), [0xA3]);
    assert_eq!(id(SEGMENT), [0x18, 0x53, 0x80, 0x67]);
}
