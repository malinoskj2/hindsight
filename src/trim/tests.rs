use super::*;
use crate::replay::{Place, TrackKind, VideoSample};

const SECOND: i64 = CLOCK_RATE;

fn frame(number: i64) -> i64 {
    number * SECOND / 60
}

/// Ten frames at 60 fps with keyframes at 0 and 5, each sample `size` bytes.
fn video(size: u32) -> Video {
    let samples = (0..10)
        .map(|number| VideoSample {
            place: Place { offset: 0, size },
            time: frame(number),
            keyframe: number % 5 == 0,
        })
        .collect();
    Video::of_samples(60, samples)
}

fn sound(start: i64, frames: usize) -> Sound {
    Sound {
        kind: TrackKind::System,
        sample_rate: 48_000,
        channels: 2,
        config: Box::new([0x11, 0x90]),
        frames: vec![Place { offset: 0, size: 1 }; frames],
        start,
    }
}

#[test]
fn a_cut_decodes_from_the_keyframe_before_it_to_its_last_picture() {
    let video = video(1);
    let cut = |start, end| Cut {
        start: frame(start),
        end: frame(end),
    };
    assert_eq!(cut(6, 8).decoded(&video), 5..8);
    assert_eq!(cut(5, 6).decoded(&video), 5..6);
    assert_eq!(cut(0, 10).decoded(&video), 0..10);
    assert_eq!(cut(4, 7).decoded(&video), 0..7);
    assert_eq!(cut(20, 30).decoded(&video), 0..0);
    assert!(cut(2, 4).shows(frame(3)) && !cut(2, 4).shows(frame(4)));
}

#[test]
fn the_cut_numbers_its_pictures_from_its_start_at_the_frame_rate() {
    let cut = Cut {
        start: frame(6),
        end: frame(10),
    };
    assert_eq!(cut.frame_index(frame(6), 60), 0);
    assert_eq!(cut.frame_index(frame(9), 60), 3);
    assert_eq!(
        cut.frame_index(frame(9) - 1, 60),
        3,
        "rounded to the nearest tick"
    );
    assert_eq!(cut.frame_index(frame(0), 60), 0);
}

#[test]
fn the_sound_keeps_a_frame_before_the_cut_and_skips_its_lead() {
    let sound = sound(-1500, 100);
    let cut = Cut {
        start: SECOND,
        end: 2 * SECOND,
    };
    assert_eq!(cut.sound(&sound), Some((47..96, 1372)));
    let whole = Cut {
        start: 0,
        end: 2 * SECOND,
    };
    assert_eq!(
        whole.sound(&sound),
        Some((0..96, 1500)),
        "the clip's own lead"
    );
    let past = Cut {
        start: 10 * SECOND,
        end: 11 * SECOND,
    };
    assert_eq!(past.sound(&sound), None);
}

#[test]
fn the_sound_read_for_a_cut_is_its_frames_in_order() {
    let mut file = std::io::Cursor::new((0..=255).collect::<Vec<u8>>());
    let mut sound = sound(0, 4);
    sound.frames = (0..4)
        .map(|number| Place {
            offset: number * 10,
            size: 2,
        })
        .collect();
    let cut = Cut {
        start: 0,
        end: 3 * i64::from(FRAME_SAMPLES) * SECOND / 48_000,
    };
    let clip = cut.read_sound(&mut file, &sound).unwrap().unwrap();
    let frames: Vec<&[u8]> = clip.frames.iter().map(|frame| &frame[..]).collect();
    assert_eq!(frames, [&[0, 1][..], &[10, 11], &[20, 21]]);
    assert_eq!((clip.lead, clip.sample_rate, clip.channels), (0, 48_000, 2));
}

#[test]
fn the_size_a_cut_s_sound_takes_counts_the_frames_it_keeps() {
    let frames = |count: i64| count * i64::from(FRAME_SAMPLES) * SECOND / 48_000;
    let cut = Cut {
        start: frames(4) + 100,
        end: frames(6) - 100,
    };
    // Frames 3 to 5 of each: the one before the cut and the two in it.
    assert_eq!(cut.sound_bytes(&[sound(0, 10), sound(0, 10)]), 6);
    assert_eq!(cut.sound_bytes(&[sound(0, 4)]), 1);
    assert_eq!(cut.sound_bytes(&[]), 0);
    assert_eq!(cut.duration(), frames(6) - frames(4) - 200);
}

#[test]
fn a_rate_is_kept_only_within_its_kind_s_range() {
    assert_eq!(Rate::new(false, 50), Some(Rate::Bitrate(50)));
    assert_eq!(Rate::new(true, 20), Some(Rate::Size(20)));
    assert_eq!(Rate::new(false, 0), None);
    assert_eq!(Rate::new(false, Rate::BITRATE_RANGE.1 + 1), None);
    assert_eq!(
        Rate::new(true, Rate::SIZE_RANGE.1),
        Some(Rate::Size(Rate::SIZE_RANGE.1))
    );
    assert_eq!(Rate::new(true, 0), None);
}

fn pixel(y: u8, u: u8, v: u8, matrix: Matrix) -> [u8; 4] {
    let nv12 = [y, y, y, y, u, v];
    let mut bgra = [0; 16];
    nv12_to_bgra(&nv12, 2, (2, 2), matrix, &mut bgra);
    assert!(bgra.chunks(4).all(|each| each == &bgra[..4]));
    bgra[..4].try_into().unwrap()
}

#[test]
fn limited_range_luma_spans_black_to_white() {
    for matrix in [Matrix::Bt709, Matrix::Bt601] {
        assert_eq!(pixel(16, 128, 128, matrix), [0, 0, 0, 255]);
        assert_eq!(pixel(235, 128, 128, matrix), [255, 255, 255, 255]);
        assert_eq!(pixel(126, 128, 128, matrix), [128, 128, 128, 255]);
        assert_eq!(pixel(0, 128, 128, matrix), [0, 0, 0, 255], "clamped");
    }
}

#[test]
fn primaries_convert_by_the_streams_matrix() {
    // Pure red in limited range: BT.709 (63, 102, 240), BT.601 (81, 90, 240).
    let close = |found: [u8; 4], wanted: [u8; 3]| {
        found[..3]
            .iter()
            .zip(wanted)
            .all(|(&found, wanted)| found.abs_diff(wanted) <= 2)
    };
    assert!(close(pixel(63, 102, 240, Matrix::Bt709), [0, 0, 255]));
    assert!(close(pixel(81, 90, 240, Matrix::Bt601), [0, 0, 255]));
    assert!(!close(pixel(63, 102, 240, Matrix::Bt601), [0, 0, 255]));
}

#[test]
fn a_picture_is_scaled_down_to_fit_keeping_its_shape_in_even_sizes() {
    assert_eq!(fit_within((1920, 1080), Some((576, 324))), (576, 324));
    assert_eq!(fit_within((2560, 1080), Some((576, 324))), (576, 242));
    assert_eq!(fit_within((1080, 1920), Some((576, 324))), (182, 324));
    assert_eq!(
        fit_within((1280, 720), Some((1920, 1080))),
        (1280, 720),
        "never enlarged"
    );
    assert_eq!(fit_within((1365, 767), None), (1364, 766));
    assert_eq!(fit_within((0, 0), Some((10, 10))), (2, 2));
}

fn timeline() -> Timeline {
    let samples = [0, 1, 2, 3, 6, 7]
        .into_iter()
        .map(|number| VideoSample {
            place: Place { offset: 0, size: 1 },
            time: frame(number),
            keyframe: number == 0,
        })
        .collect();
    Timeline::of(&Video::of_samples(60, samples))
}

#[test]
fn the_timeline_finds_the_picture_shown_at_any_time() {
    let timeline = timeline();
    assert_eq!(timeline.frame_at(frame(2) + 1), frame(2));
    assert_eq!(
        timeline.frame_at(frame(5)),
        frame(3),
        "a skipped tick holds the picture"
    );
    assert_eq!(timeline.frame_at(-5), 0);
    assert_eq!(timeline.frame_at(frame(100)), frame(7));
    assert_eq!(timeline.end_of(frame(3)), frame(6));
    assert_eq!(timeline.end_of(frame(7)), timeline.duration());
    assert_eq!(
        timeline.whole(),
        Cut {
            start: 0,
            end: timeline.duration()
        }
    );
}

#[test]
fn the_timeline_steps_by_pictures_and_snaps_to_their_edges() {
    let timeline = timeline();
    assert_eq!(timeline.step(frame(2), 1), frame(3));
    assert_eq!(timeline.step(frame(3), 1), frame(6));
    assert_eq!(timeline.step(frame(6), -3), frame(1));
    assert_eq!(timeline.step(frame(1), -5), 0);
    assert_eq!(timeline.step(frame(6), 9), frame(7));
    assert_eq!(timeline.nearest_edge(frame(4)), frame(3));
    assert_eq!(timeline.nearest_edge(frame(5)), frame(6));
    assert_eq!(timeline.nearest_edge(frame(100)), timeline.duration());
    assert_eq!(timeline.nearest_edge(-100), 0);
}
