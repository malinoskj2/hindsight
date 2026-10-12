//! The sound beside the video: the system's sound as 16-bit PCM, numbered on one clock with the
//! video's frames, kept for as long as the video ring keeps its packets, and cut to the video's
//! first and last frame when a clip is saved, so that both start and end together. The save
//! encodes the cut to AAC, so recording only keeps the samples and never runs the encoder, for
//! 192 kB a second of memory (58 MB for five minutes) where the AAC took 24 kB.
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
//! `n`, the `n`th of the run, stands for `anchor + n / sample_rate` seconds. The capture keeps
//! that true: it compares the counter time WASAPI stamps on each packet with the time the next
//! sample is due, and when they differ by more than a few milliseconds it leaves a gap (nothing
//! played, a glitch, a device change), which reads as silence, or drops the overlapping samples
//! (the sound card's clock ran fast). The sound card's crystal, which runs some tens of parts
//! per million off the counter, so never accumulates drift: the two clocks stay within that
//! tolerance however long the session, well inside one video frame at 240 fps.
//!
//! The AAC frames a save makes are numbered as if the whole run had gone through the encoder
//! from sample 0. Frame `k` decodes to encoder output samples `1024k` to `1024k + 1023`, and the
//! encoder puts `priming` samples in front of its input, so frame `k` starts at input sample
//! `1024k - priming`, at `anchor + (1024k - priming) / sample_rate`. An encoder started at input
//! sample `1024f` makes frame `f` first, so a save encodes from there.
//! A save converts the clock times of the video's first frame and of the end of its last into
//! output sample positions, takes the frames that cover them (and one before, which the decoder
//! needs to rebuild the first), and tells the MP4 edit list how many samples of the first frames
//! play before the video starts.

use std::{collections::VecDeque, sync::Arc};

/// Ticks of the shared clock per second: the 100 ns unit of WASAPI's counter positions.
pub(crate) const CLOCK_RATE: i64 = 10_000_000;
/// Samples per channel in one AAC-LC frame.
pub(crate) const FRAME_SAMPLES: u32 = 1024;
/// Seconds of sound kept while the video has not started, or the clock that relates them to it
/// is not known yet: more than any session takes to start.
const SECONDS_BEFORE_VIDEO: u64 = 10;

/// The clock time of video frame `index` of a session whose frame clock started at `origin` and
/// ticks `frame_rate` times a second.
pub(crate) fn tick_time(origin: i64, frame_rate: u32, index: u64) -> i64 {
    let offset = i128::from(index) * i128::from(CLOCK_RATE) / i128::from(frame_rate.max(1));
    origin.saturating_add(i64::try_from(offset).unwrap_or(i64::MAX))
}

/// The sound's shape and where its samples sit on the clock, fixed for a session.
#[derive(Debug)]
pub(crate) struct AudioFormat {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The clock time of the run's first sample.
    pub(crate) anchor: i64,
    /// The samples the encoder a save uses puts in front of its first input sample.
    pub(crate) priming: u32,
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

    /// The input sample an encoder must start at to make frame `first` its first.
    fn frame_input(first: i64) -> i64 {
        first.saturating_mul(i64::from(FRAME_SAMPLES))
    }
}

/// A run of interleaved samples the capture delivered in one go, from input sample `start` on.
/// The front of `data` up to `offset` is no longer part of the run: trimming moves the offset
/// rather than copying the rest.
#[derive(Clone, Debug)]
struct Chunk {
    start: u64,
    data: Arc<[i16]>,
    offset: usize,
}

impl Chunk {
    /// The samples per channel still in the chunk.
    fn length(&self, channels: usize) -> u64 {
        ((self.data.len() - self.offset) / channels) as u64
    }

    /// The input sample just past the chunk's last.
    fn end(&self, channels: usize) -> u64 {
        self.start + self.length(channels)
    }

    /// The chunk's samples, interleaved.
    fn samples(&self) -> &[i16] {
        &self.data[self.offset..]
    }
}

/// The buffered sound, oldest first. Sample numbers only increase, but may skip ahead: where
/// nothing was captured (silence, a device being switched) nothing is kept, and a save encodes
/// silence there.
#[derive(Default)]
pub(crate) struct AudioRing {
    chunks: VecDeque<Chunk>,
    /// Interleaved samples per sample of the run, as the format says.
    channels: usize,
}

impl AudioRing {
    /// Appends `data`, interleaved samples of `format`'s channels, as the run from input sample
    /// `start` on. Samples that do not come after the newest are dropped.
    pub(crate) fn push(&mut self, format: &AudioFormat, start: u64, data: Arc<[i16]>) {
        self.channels = usize::from(format.channels.max(1));
        let mut chunk = Chunk {
            start,
            data,
            offset: 0,
        };
        let end = self.end();
        if end > chunk.start {
            let overlap = usize::try_from(end - chunk.start).unwrap_or(usize::MAX);
            chunk.offset = overlap.saturating_mul(self.channels).min(chunk.data.len());
            chunk.start = end;
        }
        if chunk.length(self.channels) > 0 {
            self.chunks.push_back(chunk);
        }
    }

    /// Drops the samples numbered below `sample`, to the sample.
    pub(crate) fn drop_before(&mut self, sample: u64) {
        let channels = self.channels.max(1);
        while let Some(oldest) = self.chunks.front_mut() {
            if oldest.end(channels) <= sample {
                self.chunks.pop_front();
                continue;
            }
            if oldest.start < sample {
                let cut = usize::try_from(sample - oldest.start).unwrap_or(usize::MAX);
                oldest.offset += cut * channels;
                oldest.start = sample;
            }
            break;
        }
    }

    /// Trims the samples a clip of video starting at clock time `start` cannot need, or, while
    /// that is not known, keeps the newest few seconds.
    pub(crate) fn trim(&mut self, format: &AudioFormat, start: Option<i64>) {
        let keep_from = match start {
            Some(start) => {
                u64::try_from(AudioFormat::frame_input(format.first_frame(start))).unwrap_or(0)
            }
            None => self
                .end()
                .saturating_sub(SECONDS_BEFORE_VIDEO * u64::from(format.sample_rate)),
        };
        self.drop_before(keep_from);
    }

    /// The input sample just past the newest buffered one.
    fn end(&self) -> u64 {
        self.chunks
            .back()
            .map_or(0, |newest| newest.end(self.channels.max(1)))
    }

    /// The chunks that hold samples from input sample `sample` on; cheap, as the samples are
    /// shared rather than copied.
    fn since(&self, sample: i64) -> Vec<Chunk> {
        let channels = self.channels.max(1);
        let from = u64::try_from(sample).unwrap_or(0);
        let first = self
            .chunks
            .partition_point(|chunk| chunk.end(channels) <= from);
        self.chunks.range(first..).cloned().collect()
    }

    /// Bytes of samples currently buffered.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| size_of_val(chunk.samples()))
            .sum()
    }

    /// The buffered runs as their first sample number and length, oldest first.
    #[cfg(test)]
    pub(crate) fn runs(&self) -> Vec<(u64, u64)> {
        let channels = self.channels.max(1);
        self.chunks
            .iter()
            .map(|chunk| (chunk.start, chunk.length(channels)))
            .collect()
    }
}

/// The sound a save encodes: the samples from the start of the frame before the video's first
/// to past the end of its last, where the encoder may look ahead to finish the last frame.
#[derive(Debug)]
pub(crate) struct PcmClip {
    pub(crate) format: Arc<AudioFormat>,
    /// The input sample the encoder starts at, the first of the clip's first frame; negative
    /// when the video starts before the sound did.
    pub(crate) start: i64,
    /// The AAC frames the clip takes.
    pub(crate) frames: usize,
    /// The samples at the start of the first frames that play before the video's first frame;
    /// the edit list skips them.
    pub(crate) lead: u32,
    chunks: Vec<Chunk>,
}

impl PcmClip {
    /// Fills `output` with the interleaved samples from input sample `from` on, with silence
    /// wherever nothing was captured.
    pub(crate) fn read(&self, from: i64, output: &mut [i16]) {
        output.fill(0);
        let channels = usize::from(self.format.channels.max(1));
        let to = from.saturating_add(i64::try_from(output.len() / channels).unwrap_or(i64::MAX));
        let end_of = |chunk: &Chunk| i64::try_from(chunk.end(channels)).unwrap_or(i64::MAX);
        let first = self.chunks.partition_point(|chunk| end_of(chunk) <= from);
        // The interleaved position of sample `sample` in a run starting at sample `base`.
        let index = |sample: i64, base: i64| usize::try_from(sample - base).unwrap_or(0) * channels;
        for chunk in &self.chunks[first..] {
            let start = i64::try_from(chunk.start).unwrap_or(i64::MAX);
            if start >= to {
                break;
            }
            let (low, high) = (start.max(from), end_of(chunk).min(to));
            output[index(low, from)..index(high, from)]
                .copy_from_slice(&chunk.samples()[index(low, start)..index(high, start)]);
        }
    }
}

/// What a save writes of the sound: whole AAC frames from just before the video's first frame
/// to just past the end of its last.
#[derive(Debug)]
pub(crate) struct AudioClip {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The `AudioSpecificConfig` that the `esds` box carries.
    pub(crate) config: Box<[u8]>,
    pub(crate) frames: Vec<Arc<[u8]>>,
    /// The samples at the start of the first frames that play before the video's first frame;
    /// the edit list skips them.
    pub(crate) lead: u32,
}

/// Cuts the samples that cover the clock times from `start` to `end` out of `ring`, for a save
/// to encode.
pub(crate) fn cut(ring: &AudioRing, format: &Arc<AudioFormat>, start: i64, end: i64) -> PcmClip {
    let samples = i64::from(FRAME_SAMPLES);
    let first_sample = format.sample_at(start);
    let end_sample = format.sample_at(end).max(first_sample);
    let first = format.first_frame(start);
    // Rounded up, so the last frame reaches at least to the end of the video.
    let end_frame = end_sample.div_euclid(samples) + i64::from(end_sample % samples != 0);
    let input = AudioFormat::frame_input(first);
    PcmClip {
        format: Arc::clone(format),
        start: input,
        frames: usize::try_from(end_frame - first).unwrap_or(0),
        lead: u32::try_from(first_sample - first * samples).unwrap_or(0),
        chunks: ring.since(input),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stereo format whose input sample 0 sits at clock time 0, with an encoder delay of
    /// `priming`.
    pub(crate) fn format(priming: u32) -> Arc<AudioFormat> {
        Arc::new(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            anchor: 0,
            priming,
        })
    }

    /// A clip of 48 kHz stereo AAC-LC made of `frames`, whose first `lead` samples play before
    /// the video.
    pub(crate) fn aac_clip(frames: Vec<Arc<[u8]>>, lead: u32) -> AudioClip {
        AudioClip {
            sample_rate: 48_000,
            channels: 2,
            config: Box::new([0x11, 0x90]),
            frames,
            lead,
        }
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

    /// `length` stereo samples from input sample `start` on, each holding its own number (modulo
    /// 30 000) on the left and its negation on the right, so a read shows where each came from.
    pub(crate) fn numbered(start: u64, length: u64) -> Arc<[i16]> {
        (start..start + length)
            .flat_map(|sample| {
                let value = i16::try_from(sample % 30_000).unwrap();
                [value, -value]
            })
            .collect()
    }

    /// The clock time of input sample `sample` at 48 kHz with the anchor at 0; exact for
    /// multiples of three samples (625 ticks).
    fn time_of(sample: i64) -> i64 {
        sample * CLOCK_RATE / 48_000
    }

    /// The left channel of `length` samples of `clip` from input sample `from` on.
    fn left(clip: &PcmClip, from: i64, length: usize) -> Vec<i16> {
        let mut output = vec![1; length * 2];
        clip.read(from, &mut output);
        output.chunks(2).map(|pair| pair[0]).collect()
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
    fn keeps_the_samples_a_clip_of_the_buffered_video_needs_to_the_sample() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        for chunk in 0..20 {
            ring.push(&format, chunk * 1000, numbered(chunk * 1000, 1000));
        }
        // Video from output sample 1024 × 10 + 100 on needs frame 10, and frame 9 before it,
        // which an encoder started at input sample 9216 makes first.
        ring.trim(&format, Some(time_of(10 * 1024 + 100) - time_of(1024)));
        let runs = ring.runs();
        assert_eq!(runs[0], (9216, 784));
        assert_eq!(runs[1], (10_000, 1000));
        assert_eq!(runs.len(), 11);
        assert_eq!(ring.bytes(), (20_000 - 9216) * 4);

        // Samples that overlap the newest are dropped; the rest of a chunk is kept.
        ring.push(&format, 19_500, numbered(19_500, 1000));
        assert_eq!(ring.runs().last(), Some(&(20_000, 500)));
        ring.push(&format, 19_000, numbered(19_000, 100));
        assert_eq!(ring.runs().len(), 12);
    }

    #[test]
    fn keeps_a_few_seconds_until_the_video_starts() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        let second = 48_000;
        for chunk in 0..(SECONDS_BEFORE_VIDEO + 2) {
            ring.push(&format, chunk * second, numbered(chunk * second, second));
            ring.trim(&format, None);
        }
        let runs = ring.runs();
        assert_eq!(runs.len(), usize::try_from(SECONDS_BEFORE_VIDEO).unwrap());
        assert_eq!(runs[0].0, 2 * second);
    }

    #[test]
    fn a_cut_covers_the_video_to_the_sample_and_reads_gaps_as_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        // Sound up to input sample 7000, a gap, then sound from 12 000 to 40 000.
        ring.push(&format, 0, numbered(0, 7000));
        ring.push(&format, 12_000, numbered(12_000, 28_000));
        // The video plays from input sample 4800 to 19200: output samples 5824 to 20224.
        let clip = cut(&ring, &format, time_of(4800), time_of(19_200));

        // Frame 5 holds output sample 5824; frame 4 comes first, made by an encoder started
        // at input sample 4096, and frame 19 holds 20223.
        assert_eq!(clip.start, 4096);
        assert_eq!(clip.frames, 20 - 4);
        assert_eq!(clip.lead, 5824 - 4 * 1024);
        let covered = i64::try_from(clip.frames).unwrap() * 1024 - i64::from(clip.lead);
        assert!(covered >= 14_400);
        assert!(covered < 14_400 + 1024);

        let samples = left(&clip, 4096, 10_000);
        assert_eq!(samples[0], 4096);
        assert_eq!(samples[6999 - 4096], 6999);
        assert!(samples[7000 - 4096..12_000 - 4096].iter().all(|&s| s == 0));
        assert_eq!(samples[12_000 - 4096], 12_000);
        let mut pair = [0; 2];
        clip.read(13_000, &mut pair);
        assert_eq!(pair, [13_000, -13_000]);
        // Past the newest sample there is only silence.
        assert!(left(&clip, 39_990, 20)[10..].iter().all(|&s| s == 0));
    }

    #[test]
    fn video_before_the_first_sound_is_padded_with_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        ring.push(&format, 0, numbered(0, 2048));
        // The video starts 0.1 s before the anchor and ends 0.05 s after it.
        let clip = cut(&ring, &format, -time_of(4800), time_of(2400));

        assert!(clip.start < 0);
        let samples = left(&clip, clip.start, 8000);
        let zero = usize::try_from(-clip.start).unwrap();
        assert!(samples[..zero].iter().all(|&s| s == 0));
        assert_eq!(samples[zero..zero + 2048], *left(&clip, 0, 2048));
        assert_eq!(samples[zero + 1], 1);
        assert!(samples[zero + 2048..].iter().all(|&s| s == 0));
        let covered = i64::try_from(clip.frames).unwrap() * 1024 - i64::from(clip.lead);
        assert!(covered >= 7200);
    }
}
