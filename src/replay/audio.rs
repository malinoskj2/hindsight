//! The system's sound as 16-bit PCM, numbered on the video's QPC clock, kept as long as the
//! video ring keeps its packets and cut to the video's first and last frame when a clip is saved.

use std::{
    collections::VecDeque,
    num::{NonZeroU16, NonZeroU32},
    sync::Arc,
};

/// 100 ns ticks, the unit WASAPI stamps its packets in.
pub(crate) const CLOCK_RATE: i64 = 10_000_000;
pub(crate) const FRAME_SAMPLES: u32 = 1024;
const SECONDS_BEFORE_VIDEO: u64 = 10;

/// Clock ticks in `samples` at `rate` a second, saturating.
pub(crate) fn ticks_of(samples: impl Into<i128>, rate: u32) -> i64 {
    saturate(samples.into() * i128::from(CLOCK_RATE) / i128::from(rate))
}

/// Samples at `rate` a second in `ticks`, rounded down, saturating.
pub(crate) fn samples_of(ticks: i64, rate: u32) -> i64 {
    saturate((i128::from(ticks) * i128::from(rate)).div_euclid(i128::from(CLOCK_RATE)))
}

fn saturate(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

pub(crate) fn tick_time(origin: i64, frame_rate: u32, index: u64) -> i64 {
    origin.saturating_add(ticks_of(index, frame_rate))
}

#[derive(Debug)]
pub(crate) struct AudioFormat {
    pub(crate) sample_rate: NonZeroU32,
    pub(crate) channels: NonZeroU16,
    /// Clock time of input sample 0.
    pub(crate) anchor: i64,
    /// Samples the AAC encoder puts in front of its input.
    pub(crate) priming: u32,
}

impl AudioFormat {
    /// Encoder output sample playing at `time`; negative before the stream began.
    fn sample_at(&self, time: i64) -> i64 {
        samples_of(time.saturating_sub(self.anchor), self.sample_rate.get())
            .saturating_add(i64::from(self.priming))
    }

    #[cfg(test)]
    pub(crate) fn frame_time(&self, number: u64) -> i64 {
        let input = i128::from(number) * i128::from(FRAME_SAMPLES) - i128::from(self.priming);
        self.anchor
            .saturating_add(ticks_of(input, self.sample_rate.get()))
    }

    /// The frame before the one playing at `start`; the decoder needs it to rebuild that one.
    fn first_frame(&self, start: i64) -> i64 {
        self.sample_at(start)
            .div_euclid(i64::from(FRAME_SAMPLES))
            .saturating_sub(1)
    }

    fn frame_input(first: i64) -> i64 {
        first.saturating_mul(i64::from(FRAME_SAMPLES))
    }

    fn channels(&self) -> usize {
        usize::from(self.channels.get())
    }
}

/// Trimming advances `offset` instead of copying.
#[derive(Clone, Debug)]
struct Chunk {
    start: u64,
    data: Arc<[i16]>,
    offset: usize,
}

impl Chunk {
    fn length(&self, channels: usize) -> u64 {
        ((self.data.len() - self.offset) / channels) as u64
    }

    fn end(&self, channels: usize) -> u64 {
        self.start + self.length(channels)
    }

    fn samples(&self) -> &[i16] {
        &self.data[self.offset..]
    }
}

/// Oldest first; gaps where nothing was captured read as silence.
#[derive(Default)]
pub(crate) struct AudioRing {
    chunks: VecDeque<Chunk>,
}

impl AudioRing {
    pub(crate) fn push(&mut self, format: &AudioFormat, start: u64, data: Arc<[i16]>) {
        let channels = format.channels();
        let mut chunk = Chunk {
            start,
            data,
            offset: 0,
        };
        let end = self.end(channels);
        if end > chunk.start {
            let overlap = usize::try_from(end - chunk.start).unwrap_or(usize::MAX);
            chunk.offset = overlap.saturating_mul(channels).min(chunk.data.len());
            chunk.start = end;
        }
        if chunk.length(channels) > 0 {
            self.chunks.push_back(chunk);
        }
    }

    fn drop_before(&mut self, channels: usize, sample: u64) {
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

    pub(crate) fn trim(&mut self, format: &AudioFormat, start: Option<i64>) {
        let channels = format.channels();
        let keep_from = match start {
            Some(start) => {
                u64::try_from(AudioFormat::frame_input(format.first_frame(start))).unwrap_or(0)
            }
            None => self
                .end(channels)
                .saturating_sub(SECONDS_BEFORE_VIDEO * u64::from(format.sample_rate.get())),
        };
        self.drop_before(channels, keep_from);
    }

    fn end(&self, channels: usize) -> u64 {
        self.chunks.back().map_or(0, |newest| newest.end(channels))
    }

    fn since(&self, format: &AudioFormat, sample: i64) -> Vec<Chunk> {
        let channels = format.channels();
        let from = u64::try_from(sample).unwrap_or(0);
        let first = self
            .chunks
            .partition_point(|chunk| chunk.end(channels) <= from);
        self.chunks.range(first..).cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| size_of_val(chunk.samples()))
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn runs(&self, format: &AudioFormat) -> Vec<(u64, u64)> {
        let channels = format.channels();
        self.chunks
            .iter()
            .map(|chunk| (chunk.start, chunk.length(channels)))
            .collect()
    }
}

#[derive(Debug)]
pub(crate) struct PcmClip {
    pub(crate) format: Arc<AudioFormat>,
    /// Negative when the video starts before the sound did.
    pub(crate) start: i64,
    pub(crate) frames: usize,
    /// Samples of the first frames that play before the video; the edit list skips them.
    pub(crate) lead: u32,
    chunks: Vec<Chunk>,
}

impl PcmClip {
    pub(crate) fn read(&self, from: i64, output: &mut [i16]) {
        output.fill(0);
        let channels = self.format.channels();
        let to = from.saturating_add(i64::try_from(output.len() / channels).unwrap_or(i64::MAX));
        let end_of = |chunk: &Chunk| i64::try_from(chunk.end(channels)).unwrap_or(i64::MAX);
        let first = self.chunks.partition_point(|chunk| end_of(chunk) <= from);
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

#[derive(Debug)]
pub(crate) struct AudioClip {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    /// The `AudioSpecificConfig` the `esds` box carries.
    pub(crate) config: Box<[u8]>,
    pub(crate) frames: Vec<Arc<[u8]>>,
    /// Samples of the first frames that play before the video; the edit list skips them.
    pub(crate) lead: u32,
}

pub(crate) fn cut(ring: &AudioRing, format: &Arc<AudioFormat>, start: i64, end: i64) -> PcmClip {
    let samples = i64::from(FRAME_SAMPLES);
    let first_sample = format.sample_at(start);
    let end_sample = format.sample_at(end).max(first_sample);
    let first = format.first_frame(start);
    let end_frame = end_sample.div_euclid(samples) + i64::from(end_sample % samples != 0);
    let input = AudioFormat::frame_input(first);
    PcmClip {
        format: Arc::clone(format),
        start: input,
        frames: usize::try_from(end_frame - first).unwrap_or(0),
        lead: u32::try_from(first_sample - first * samples).unwrap_or(0),
        chunks: ring.since(format, input),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const RATE: NonZeroU32 = NonZeroU32::new(48_000).unwrap();
    pub(crate) const STEREO: NonZeroU16 = NonZeroU16::new(2).unwrap();

    pub(crate) fn format(priming: u32) -> Arc<AudioFormat> {
        Arc::new(AudioFormat {
            sample_rate: RATE,
            channels: STEREO,
            anchor: 0,
            priming,
        })
    }

    pub(crate) fn aac_clip(frames: Vec<Arc<[u8]>>, lead: u32) -> AudioClip {
        AudioClip {
            sample_rate: 48_000,
            channels: 2,
            config: Box::new([0x11, 0x90]),
            frames,
            lead,
        }
    }

    pub(crate) fn encoded_fixture() -> Vec<Arc<[u8]>> {
        crate::audio::testing::frames(include_bytes!("../../tests/fixtures/aac.adts"))
    }

    pub(crate) fn numbered(start: u64, length: u64) -> Arc<[i16]> {
        (start..start + length)
            .flat_map(|sample| {
                let value = i16::try_from(sample % 30_000).unwrap();
                [value, -value]
            })
            .collect()
    }

    fn time_of(sample: i64) -> i64 {
        sample * CLOCK_RATE / 48_000
    }

    fn left(clip: &PcmClip, from: i64, length: usize) -> Vec<i16> {
        let mut output = vec![1; length * 2];
        clip.read(from, &mut output);
        output.chunks(2).map(|pair| pair[0]).collect()
    }

    #[test]
    fn video_ticks_and_audio_frames_share_the_clock() {
        assert_eq!(tick_time(1_000, 60, 0), 1_000);
        assert_eq!(tick_time(1_000, 60, 60), 1_000 + CLOCK_RATE);
        assert_eq!(tick_time(0, 60, 18_000), 300 * CLOCK_RATE);

        let format = format(1024);
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
        ring.trim(&format, Some(time_of(10 * 1024 + 100) - time_of(1024)));
        let runs = ring.runs(&format);
        assert_eq!(runs[0], (9216, 784));
        assert_eq!(runs[1], (10_000, 1000));
        assert_eq!(runs.len(), 11);
        assert_eq!(ring.bytes(), (20_000 - 9216) * 4);

        ring.push(&format, 19_500, numbered(19_500, 1000));
        assert_eq!(ring.runs(&format).last(), Some(&(20_000, 500)));
        ring.push(&format, 19_000, numbered(19_000, 100));
        assert_eq!(ring.runs(&format).len(), 12);
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
        let runs = ring.runs(&format);
        assert_eq!(runs.len(), usize::try_from(SECONDS_BEFORE_VIDEO).unwrap());
        assert_eq!(runs[0].0, 2 * second);
    }

    #[test]
    fn a_cut_covers_the_video_to_the_sample_and_reads_gaps_as_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        ring.push(&format, 0, numbered(0, 7000));
        ring.push(&format, 12_000, numbered(12_000, 28_000));
        let clip = cut(&ring, &format, time_of(4800), time_of(19_200));

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
        assert!(left(&clip, 39_990, 20)[10..].iter().all(|&s| s == 0));
    }

    #[test]
    fn video_before_the_first_sound_is_padded_with_silence() {
        let format = format(1024);
        let mut ring = AudioRing::default();
        ring.push(&format, 0, numbered(0, 2048));
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
