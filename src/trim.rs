//! Trimming a clip: the samples a cut between two times needs, the sound it keeps, the bitrate
//! it is encoded at, and its pictures converted for display.

use crate::replay::{
    AudioClip, CLOCK_RATE, FRAME_SAMPLES, Matrix, Sound, Video, read_place, samples_of,
};
use std::{
    io::{self, Read, Seek},
    ops::Range,
    sync::Arc,
};

/// Bits per second below which a cut is never encoded.
const MIN_BITRATE: u64 = 2_000_000;
/// The fixed point the colour conversion computes in.
const FRACTION_BITS: u32 = 16;
const LIMITED_BLACK: i32 = 16;
const LIMITED_LUMA_RANGE: f64 = 219.0;
const LIMITED_CHROMA_RANGE: f64 = 224.0;
const CHROMA_ZERO: i32 = 128;

/// A stretch of a clip, in clock ticks from its first picture: the pictures shown from `start`
/// up to, but not including, `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cut {
    pub(crate) start: i64,
    pub(crate) end: i64,
}

impl Cut {
    pub(crate) fn shows(&self, time: i64) -> bool {
        (self.start..self.end).contains(&time)
    }

    /// The samples to decode, in decode order: from the keyframe the first picture needs to the
    /// last sample shown in the cut.
    pub(crate) fn decoded(&self, video: &Video) -> Range<usize> {
        let Some(last) = video
            .samples
            .iter()
            .rposition(|sample| self.shows(sample.time))
        else {
            return 0..0;
        };
        video.keyframe_before(self.start).min(last)..last + 1
    }

    /// The picture's tick on the cut's frame clock, which starts at 0 with the cut.
    pub(crate) fn frame_index(&self, time: i64, frame_rate: u32) -> u64 {
        let ticks = i128::from(time.saturating_sub(self.start).max(0)) * i128::from(frame_rate);
        let clock = i128::from(CLOCK_RATE);
        u64::try_from((ticks + clock / 2) / clock).unwrap_or(u64::MAX)
    }

    /// The sound's frames that play during the cut, and one before them, which an AAC decoder
    /// needs to decode the first one whole, with the samples of them that play before the cut.
    pub(crate) fn sound(&self, sound: &Sound) -> Option<(Range<usize>, u32)> {
        let rate = sound.sample_rate;
        let (start, end) = (samples_of(self.start, rate), samples_of(self.end, rate));
        let frame = i64::from(FRAME_SAMPLES);
        let count = i64::try_from(sound.frames.len()).ok()?;
        let first = ((start - sound.start).div_euclid(frame) - 1).clamp(0, count);
        let last = (end - sound.start + frame - 1)
            .div_euclid(frame)
            .clamp(0, count);
        if first >= last {
            return None;
        }
        let lead = (start - (sound.start + first * frame)).max(0);
        Some((
            usize::try_from(first).ok()?..usize::try_from(last).ok()?,
            u32::try_from(lead).ok()?,
        ))
    }

    /// Reads the sound's frames in the cut, ready for the writers.
    pub(crate) fn read_sound(
        &self,
        file: &mut (impl Read + Seek),
        sound: &Sound,
    ) -> io::Result<Option<AudioClip>> {
        let Some((frames, lead)) = self.sound(sound) else {
            return Ok(None);
        };
        let frames = sound.frames[frames]
            .iter()
            .map(|&place| read_place(file, place).map(Arc::from))
            .collect::<io::Result<_>>()?;
        Ok(Some(AudioClip {
            kind: sound.kind,
            sample_rate: sound.sample_rate,
            channels: sound.channels,
            config: sound.config.clone(),
            frames,
            lead,
        }))
    }
}

/// The times of a clip's pictures in display order, which the trim view moves between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Timeline {
    times: Vec<i64>,
    duration: i64,
}

impl Timeline {
    pub(crate) fn of(video: &Video) -> Self {
        let mut times: Vec<i64> = video.samples.iter().map(|sample| sample.time).collect();
        times.sort_unstable();
        times.dedup();
        let duration = video.duration.max(times.last().map_or(0, |last| last + 1));
        Self { times, duration }
    }

    pub(crate) const fn duration(&self) -> i64 {
        self.duration
    }

    pub(crate) fn whole(&self) -> Cut {
        Cut {
            start: self.times.first().copied().unwrap_or(0),
            end: self.duration,
        }
    }

    /// The time of the picture shown at `time`: the last one at or before it, or the first.
    pub(crate) fn frame_at(&self, time: i64) -> i64 {
        let after = self.times.partition_point(|&shown| shown <= time);
        self.times
            .get(after.saturating_sub(1))
            .copied()
            .unwrap_or(0)
    }

    /// The time of the picture `frames` away from the one shown at `time`, held to the clip.
    pub(crate) fn step(&self, time: i64, frames: isize) -> i64 {
        let current = self
            .times
            .partition_point(|&shown| shown <= time)
            .saturating_sub(1);
        let target = current
            .saturating_add_signed(frames)
            .min(self.times.len().saturating_sub(1));
        self.times.get(target).copied().unwrap_or(0)
    }

    /// Where the picture shown at `time` ends: the next picture, or the end of the clip.
    pub(crate) fn end_of(&self, time: i64) -> i64 {
        let after = self.times.partition_point(|&shown| shown <= time);
        self.times.get(after).copied().unwrap_or(self.duration)
    }

    /// The edge between pictures nearest to `time`, a cut's start or end.
    pub(crate) fn nearest_edge(&self, time: i64) -> i64 {
        let after = self.times.partition_point(|&shown| shown <= time);
        let before = after
            .checked_sub(1)
            .and_then(|index| self.times.get(index))
            .copied();
        let next = self.times.get(after).copied().unwrap_or(self.duration);
        match before {
            Some(before) if time - before <= next - time => before,
            _ => next,
        }
    }
}

/// The bits per second a cut is encoded at: the bits per frame the settings record at, at the
/// clip's frame rate, but no more than the clip itself spends, since more cannot add back what
/// its encoding lost.
pub(crate) fn bitrate(recorded: u32, recorded_frame_rate: u32, video: &Video) -> u32 {
    let per_frame = u64::from(recorded) / u64::from(recorded_frame_rate.max(1));
    let wanted = per_frame * u64::from(video.stream.frame_rate);
    let spent = video_bitrate(video);
    let chosen = if spent > 0 { wanted.min(spent) } else { wanted };
    u32::try_from(chosen.max(MIN_BITRATE)).unwrap_or(u32::MAX)
}

fn video_bitrate(video: &Video) -> u64 {
    let bytes: u64 = video
        .samples
        .iter()
        .map(|sample| u64::from(sample.place.size))
        .sum();
    let duration = u64::try_from(video.duration).unwrap_or(0);
    if duration == 0 {
        return 0;
    }
    let bits = u128::from(bytes) * 8 * u128::from(CLOCK_RATE.unsigned_abs());
    u64::try_from(bits / u128::from(duration)).unwrap_or(u64::MAX)
}

/// The largest size of `size`'s shape within `bounds`, but no larger than `size`, in even
/// numbers as NV12 needs.
pub(crate) fn fit_within((width, height): (u32, u32), bounds: Option<(u32, u32)>) -> (u32, u32) {
    let (width, height) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let (fitted_width, fitted_height) = match bounds {
        None => (width, height),
        Some((bound_width, bound_height)) => {
            let (bound_width, bound_height) = (u64::from(bound_width), u64::from(bound_height));
            if width * bound_height <= height * bound_width {
                let fitted = height.min(bound_height);
                (width * fitted / height, fitted)
            } else {
                let fitted = width.min(bound_width);
                (fitted, height * fitted / width)
            }
        }
    };
    let even = |value: u64| u32::try_from(value & !1).unwrap_or(!1).max(2);
    (even(fitted_width), even(fitted_height))
}

/// Converts limited-range NV12, `pitch` bytes a row, to top-down BGRA.
pub(crate) fn nv12_to_bgra(
    nv12: &[u8],
    pitch: usize,
    (width, height): (usize, usize),
    matrix: Matrix,
    bgra: &mut [u8],
) {
    let factors = Factors::of(matrix);
    let chroma = &nv12[(pitch * height).min(nv12.len())..];
    for (row, out) in bgra.chunks_exact_mut(width * 4).take(height).enumerate() {
        let luma = nv12.get(row * pitch..row * pitch + width);
        let colour = chroma.get((row / 2) * pitch..(row / 2) * pitch + width.next_multiple_of(2));
        let (Some(luma), Some(colour)) = (luma, colour) else {
            return;
        };
        for (column, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let pair = column & !1;
            let [blue, green, red] = factors.convert(luma[column], colour[pair], colour[pair + 1]);
            *pixel = [blue, green, red, u8::MAX];
        }
    }
}

/// The conversion's multipliers in fixed point.
struct Factors {
    luma: i32,
    red_v: i32,
    green_u: i32,
    green_v: i32,
    blue_u: i32,
}

impl Factors {
    fn of(matrix: Matrix) -> Self {
        let (red, blue) = matrix.red_and_blue_weights();
        let green = 1.0 - red - blue;
        let chroma = 255.0 / LIMITED_CHROMA_RANGE;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "each multiplier is below 4 in fixed point of 16 fractional bits"
        )]
        let fixed = |value: f64| (value * f64::from(1 << FRACTION_BITS)).round() as i32;
        Self {
            luma: fixed(255.0 / LIMITED_LUMA_RANGE),
            red_v: fixed(chroma * 2.0 * (1.0 - red)),
            green_u: fixed(chroma * 2.0 * (1.0 - blue) * blue / green),
            green_v: fixed(chroma * 2.0 * (1.0 - red) * red / green),
            blue_u: fixed(chroma * 2.0 * (1.0 - blue)),
        }
    }

    fn convert(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let y = (i32::from(y) - LIMITED_BLACK) * self.luma;
        let (u, v) = (i32::from(u) - CHROMA_ZERO, i32::from(v) - CHROMA_ZERO);
        let round = 1 << (FRACTION_BITS - 1);
        let channel = |value: i32| {
            u8::try_from(((value + round) >> FRACTION_BITS).clamp(0, 255)).unwrap_or(u8::MAX)
        };
        [
            channel(y + self.blue_u * u),
            channel(y - self.green_u * u - self.green_v * v),
            channel(y + self.red_v * v),
        ]
    }
}

#[cfg(test)]
mod tests;
