//! The sound beside the video: AAC frames of what the system plays, numbered on one clock with
//! the video's frames, kept for as long as the video ring keeps its packets, and cut to the
//! video's first and last frame when a clip is saved, so that both start and end together.
//!
//! # One clock for both streams
//!
//! Both streams are placed on the performance counter (QPC), in the 100 ns units WASAPI stamps
//! its packets in ([`CLOCK_RATE`] ticks a second).
//!
//! Video frame `i` of a session is the picture taken on tick `i` of the frame clock, which
//! started at the session's origin, so it stands for `origin + i / frame_rate` seconds
//! ([`tick_time`]). That holds whether the tick brought a new picture, repeated the previous one
//! or was skipped, because the pacer derives tick numbers from the time elapsed since its start
//! rather than counting wake-ups; the video therefore never drifts from the counter. (The
//! pacer's period is rounded to whole nanoseconds, which puts tick 18 000 at 60 fps, five
//! minutes in, 12 µs early.)
//!
//! Audio is one continuous run of samples per session at the format's sample rate. Input sample
//! `n`, the `n`th handed to the encoder, stands for `anchor + n / sample_rate` seconds. The
//! capture keeps that true: it compares the counter time WASAPI stamps on each packet with the
//! time the next sample is due, and when they differ by more than a few milliseconds it inserts
//! silence (a gap: nothing played, a glitch, a device change) or drops the overlapping samples
//! (the sound card's clock ran fast). The sound card's crystal, which runs some tens of parts
//! per million off the counter, so never accumulates drift: the two clocks stay within that
//! tolerance however long the session, well inside one video frame at 240 fps.
//!
//! AAC frame `k` decodes to encoder output samples `1024k` to `1024k + 1023`, and the encoder
//! puts `priming` samples in front of its input, so frame `k` starts at input sample
//! `1024k - priming`, at `anchor + (1024k - priming) / sample_rate`.
//! A save converts the clock times of the video's first frame and of the end of its last into
//! output sample positions, takes the frames that cover them (and one before, which the decoder
//! needs to rebuild the first), and tells the MP4 edit list how many samples of the first frames
//! play before the video starts.

use std::{collections::VecDeque, sync::Arc};

/// Ticks of the shared clock per second: the 100 ns unit of WASAPI's counter positions.
pub(crate) const CLOCK_RATE: i64 = 10_000_000;
/// Samples per channel in one AAC-LC frame.
pub(crate) const FRAME_SAMPLES: u32 = 1024;
/// Frames kept while the video has not started, or the clock that relates them to it is not
/// known yet: about ten seconds at 48 kHz, more than any session takes to start.
const FRAMES_BEFORE_VIDEO: usize = 512;

/// The clock time of video frame `index` of a session whose frame clock started at `origin` and
/// ticks `frame_rate` times a second.
pub(crate) fn tick_time(origin: i64, frame_rate: u32, index: u64) -> i64 {
    let offset = i128::from(index) * i128::from(CLOCK_RATE) / i128::from(frame_rate.max(1));
    origin.saturating_add(i64::try_from(offset).unwrap_or(i64::MAX))
}

/// The encoded sound's shape and where its frames sit on the clock, fixed for a session.
#[derive(Debug)]
pub(crate) struct AudioFormat {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The `AudioSpecificConfig` that the `esds` box carries.
    pub(crate) config: Box<[u8]>,
    /// The clock time of the first sample handed to the encoder.
    pub(crate) anchor: i64,
    /// The samples the encoder puts in front of its first input sample.
    pub(crate) priming: u32,
    /// One frame of digital silence, for stretches that no captured frame covers.
    pub(crate) silence: Arc<[u8]>,
}

impl AudioFormat {
    /// The encoder output sample playing at clock time `time`, rounded down; negative before
    /// the stream began.
    fn sample_at(&self, time: i64) -> i64 {
        let elapsed = i128::from(time) - i128::from(self.anchor);
        let input = (elapsed * i128::from(self.sample_rate)).div_euclid(i128::from(CLOCK_RATE));
        i64::try_from(input)
            .unwrap_or(if input < 0 { i64::MIN } else { i64::MAX })
            .saturating_add(i64::from(self.priming))
    }

    /// The clock time at which frame `number` starts playing.
    #[cfg(test)]
    pub(crate) fn frame_time(&self, number: u64) -> i64 {
        let input = i128::from(number) * i128::from(FRAME_SAMPLES) - i128::from(self.priming);
        let offset = input * i128::from(CLOCK_RATE) / i128::from(self.sample_rate.max(1));
        self.anchor
            .saturating_add(i64::try_from(offset).unwrap_or(i64::MAX))
    }

    /// The number of the frame a clip starting at clock time `start` begins with: the one
    /// before the frame playing then, which the decoder needs to rebuild that one fully.
    fn first_frame(&self, start: i64) -> i64 {
        self.sample_at(start)
            .div_euclid(i64::from(FRAME_SAMPLES))
            .saturating_sub(1)
    }
}

/// One encoded AAC frame and its number in the session's run of frames.
#[derive(Debug)]
pub(crate) struct AudioFrame {
    pub(crate) number: u64,
    pub(crate) data: Arc<[u8]>,
}

/// The buffered frames, oldest first. Numbers only increase, but may skip ahead: a long silence
/// is not encoded, and a save fills such a gap with [`AudioFormat::silence`].
#[derive(Default)]
pub(crate) struct AudioRing {
    frames: VecDeque<AudioFrame>,
    /// Bytes of frame data in `frames`.
    bytes: usize,
}

impl AudioRing {
    /// Appends a frame; one whose number is not past the newest is ignored.
    pub(crate) fn push(&mut self, frame: AudioFrame) {
        if self
            .frames
            .back()
            .is_some_and(|newest| newest.number >= frame.number)
        {
            return;
        }
        self.bytes += frame.data.len();
        self.frames.push_back(frame);
    }

    /// Drops the frames numbered below `number`.
    pub(crate) fn drop_before(&mut self, number: u64) {
        while let Some(dropped) = self.frames.pop_front_if(|frame| frame.number < number) {
            self.bytes -= dropped.data.len();
        }
    }

    /// Drops the oldest frames until at most `count` are left.
    fn keep_newest(&mut self, count: usize) {
        while self.frames.len() > count
            && let Some(dropped) = self.frames.pop_front()
        {
            self.bytes -= dropped.data.len();
        }
    }

    /// Trims the frames a clip of video starting at clock time `start` cannot need, or, while
    /// that is not known, keeps the newest few seconds.
    pub(crate) fn trim(&mut self, format: &AudioFormat, start: Option<i64>) {
        match start {
            Some(start) => self.drop_before(u64::try_from(format.first_frame(start)).unwrap_or(0)),
            None => self.keep_newest(FRAMES_BEFORE_VIDEO),
        }
    }

    /// Frame `number`, if it is buffered.
    fn get(&self, number: u64) -> Option<&Arc<[u8]>> {
        self.frames
            .binary_search_by_key(&number, |frame| frame.number)
            .ok()
            .map(|index| &self.frames[index].data)
    }

    /// Bytes of frame data currently buffered.
    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    /// The numbers of the buffered frames, oldest first.
    #[cfg(test)]
    pub(crate) fn numbers(&self) -> Vec<u64> {
        self.frames.iter().map(|frame| frame.number).collect()
    }
}

/// What a save writes of the sound: whole frames from just before the video's first frame to
/// just past the end of its last, with silence wherever nothing was captured.
#[derive(Debug)]
pub(crate) struct AudioClip {
    pub(crate) format: Arc<AudioFormat>,
    pub(crate) frames: Vec<Arc<[u8]>>,
    /// The samples at the start of the first frames that play before the video's first frame;
    /// the edit list skips them.
    pub(crate) lead: u32,
}

/// Cuts the frames that cover the clock times from `start` to `end` out of `ring`.
pub(crate) fn cut(ring: &AudioRing, format: &Arc<AudioFormat>, start: i64, end: i64) -> AudioClip {
    let samples = i64::from(FRAME_SAMPLES);
    let first_sample = format.sample_at(start);
    let end_sample = format.sample_at(end).max(first_sample);
    let first = format.first_frame(start);
    // Rounded up, so the last frame reaches at least to the end of the video.
    let end_frame = end_sample.div_euclid(samples) + i64::from(end_sample % samples != 0);
    let frames = (first..end_frame)
        .map(|number| {
            u64::try_from(number)
                .ok()
                .and_then(|number| ring.get(number))
                .unwrap_or(&format.silence)
                .clone()
        })
        .collect();
    AudioClip {
        format: Arc::clone(format),
        frames,
        lead: u32::try_from(first_sample - first * samples).unwrap_or(0),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A format whose input sample 0 sits at clock time 0, with an encoder delay of `priming`.
    pub(crate) fn format(priming: u32) -> Arc<AudioFormat> {
        Arc::new(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            config: Box::new([0x11, 0x90]),
            anchor: 0,
            priming,
            silence: Arc::from(&b"silence"[..]),
        })
    }

    /// AAC-LC frames from ffmpeg's encoder (a 440 Hz tone for 2.5 s, 48 kHz stereo at
    /// 64 kb/s, stored as ADTS), without their seven-byte headers.
    pub(crate) fn encoded_fixture() -> Vec<Arc<[u8]>> {
        let mut rest: &[u8] = include_bytes!("../../tests/fixtures/aac.adts");
        let mut frames = Vec::new();
        while !rest.is_empty() {
            assert_eq!(
                &rest[..2],
                &[0xFF, 0xF1],
                "an ADTS header without a checksum"
            );
            let length = (usize::from(rest[3] & 0x03) << 11)
                | (usize::from(rest[4]) << 3)
                | usize::from(rest[5] >> 5);
            frames.push(Arc::from(&rest[7..length]));
            rest = &rest[length..];
        }
        frames
    }

    fn frame(number: u64) -> AudioFrame {
        AudioFrame {
            number,
            data: Arc::from(number.to_be_bytes().as_slice()),
        }
    }

    /// The clock time of input sample `sample` at 48 kHz with the anchor at 0; exact for
    /// multiples of three samples (625 ticks).
    fn time_of(sample: i64) -> i64 {
        sample * CLOCK_RATE / 48_000
    }

    #[test]
    fn video_ticks_and_audio_frames_share_the_clock() {
        assert_eq!(tick_time(1_000, 60, 0), 1_000);
        assert_eq!(tick_time(1_000, 60, 60), 1_000 + CLOCK_RATE);
        // Five minutes at 60 fps land exactly on the second, without accumulated rounding.
        assert_eq!(tick_time(0, 60, 18_000), 300 * CLOCK_RATE);

        let format = format(1024);
        // Frame 1 holds input samples 0 to 1023: it starts at the anchor.
        assert_eq!(format.frame_time(1), 0);
        assert_eq!(format.frame_time(0), -time_of(1024));
        assert_eq!(format.sample_at(0), 1024);
        assert_eq!(format.sample_at(time_of(48_000)), 48_000 + 1024);
        assert_eq!(format.sample_at(-1), 1023);
    }

    #[test]
    fn keeps_the_frames_a_clip_of_the_buffered_video_needs() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        for number in 0..20 {
            ring.push(frame(number));
        }
        // Video from output sample 1024 × 10 + 100 on needs frame 10, and frame 9 before it.
        ring.trim(&format, Some(time_of(10 * 1024 + 100) - time_of(1024)));
        assert_eq!(ring.numbers(), (9..20).collect::<Vec<_>>());
        assert_eq!(ring.bytes(), 11 * 8);

        // Out-of-order and repeated frames are ignored.
        ring.push(frame(15));
        ring.push(frame(19));
        assert_eq!(ring.numbers().len(), 11);
    }

    #[test]
    fn keeps_a_few_seconds_until_the_video_starts() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        for number in 0..(FRAMES_BEFORE_VIDEO as u64 + 10) {
            ring.push(frame(number));
            ring.trim(&format, None);
        }
        assert_eq!(ring.numbers().len(), FRAMES_BEFORE_VIDEO);
        assert_eq!(ring.numbers()[0], 10);
    }

    #[test]
    fn a_cut_covers_the_video_and_fills_gaps_with_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        for number in (0..8).chain(12..40) {
            ring.push(frame(number));
        }
        // The video plays from input sample 4800 to 19200: output samples 5824 to 20224.
        let clip = cut(&ring, &format, time_of(4800), time_of(19_200));

        // Frame 5 holds output sample 5824; frame 4 comes first, and frame 19 holds 20223.
        assert_eq!(clip.frames.len(), 20 - 4);
        assert_eq!(clip.lead, 5824 - 4 * 1024);
        assert_eq!(&*clip.frames[0], &4_u64.to_be_bytes());
        assert_eq!(&*clip.frames[4], b"silence", "frame 8 was never captured");
        assert_eq!(&*clip.frames[7], b"silence", "frame 11 was never captured");
        assert_eq!(&*clip.frames[8], &12_u64.to_be_bytes());
        let covered = i64::try_from(clip.frames.len()).unwrap() * 1024 - i64::from(clip.lead);
        assert!(covered >= 14_400);
        assert!(covered < 14_400 + 1024);
    }

    #[test]
    fn video_before_the_first_sound_is_padded_with_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        ring.push(frame(0));
        ring.push(frame(1));
        // The video starts 0.1 s before the anchor and ends 0.05 s after it.
        let clip = cut(&ring, &format, -time_of(4800), time_of(2400));

        assert_eq!(&*clip.frames[0], b"silence");
        assert!(
            clip.frames
                .iter()
                .any(|frame| **frame == 1_u64.to_be_bytes())
        );
        let covered = i64::try_from(clip.frames.len()).unwrap() * 1024 - i64::from(clip.lead);
        assert!(covered >= 7200);
    }
}
