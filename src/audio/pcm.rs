//! Converts WASAPI's float or 16-bit samples to the 16-bit stereo or mono the encoder takes.

use std::num::NonZeroU16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sample {
    Float,
    Int16,
}

impl Sample {
    const fn bytes(self) -> usize {
        match self {
            Self::Float => 4,
            Self::Int16 => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) sample: Sample,
    pub(super) channels: u16,
}

impl Layout {
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture uses it")
    )]
    pub(super) const fn frame_bytes(self) -> usize {
        self.sample.bytes() * self.channels as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Channels {
    Mono,
    Stereo,
}

impl Channels {
    pub(super) const fn count(self) -> NonZeroU16 {
        match self {
            Self::Mono => NonZeroU16::new(1).unwrap(),
            Self::Stereo => NonZeroU16::new(2).unwrap(),
        }
    }

    pub(super) fn append(self, output: &mut Vec<i16>, data: &[u8], layout: Layout) {
        let channels = usize::from(layout.channels);
        match layout.sample {
            Sample::Float => {
                self.append_frames(output, data.as_chunks::<4>().0, channels, |bytes| {
                    from_float(f32::from_le_bytes(bytes))
                });
            }
            Sample::Int16 => self.append_frames(
                output,
                data.as_chunks::<2>().0,
                channels,
                i16::from_le_bytes,
            ),
        }
    }

    fn append_frames<const N: usize>(
        self,
        output: &mut Vec<i16>,
        samples: &[[u8; N]],
        channels: usize,
        convert: impl Fn([u8; N]) -> i16,
    ) {
        let frames = samples.chunks_exact(channels);
        output.reserve(frames.len() * usize::from(self.count().get()));
        for frame in frames {
            let left = convert(frame[0]);
            let right = frame.get(1).map_or(left, |&right| convert(right));
            match self {
                Self::Mono => output.push(left.midpoint(right)),
                Self::Stereo => output.extend([left, right]),
            }
        }
    }
}

fn from_float(value: f32) -> i16 {
    // NaN survives the clamp and casts to 0.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to the i16 range first"
    )]
    let scaled = (value * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
    scaled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn float_stereo_becomes_16_bit_stereo() {
        let mut output = Vec::new();
        let layout = Layout {
            sample: Sample::Float,
            channels: 2,
        };
        Channels::Stereo.append(
            &mut output,
            &floats(&[0.0, 0.5, -1.0, 1.0, 2.0, f32::NAN]),
            layout,
        );
        assert_eq!(output, [0, 16_384, -32_768, 32_767, 32_767, 0]);
    }

    #[test]
    fn mono_is_doubled_and_extra_channels_dropped() {
        let mut output = Vec::new();
        let mono = Layout {
            sample: Sample::Int16,
            channels: 1,
        };
        Channels::Stereo.append(&mut output, &[1, 0, 0xFF, 0xFF], mono);
        assert_eq!(output, [1, 1, -1, -1]);

        output.clear();
        let surround = Layout {
            sample: Sample::Int16,
            channels: 3,
        };
        Channels::Stereo.append(&mut output, &[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0], surround);
        assert_eq!(output, [1, 2, 4, 5]);
    }

    #[test]
    fn stereo_is_averaged_for_mono_and_mono_kept() {
        let mut output = Vec::new();
        let stereo = Layout {
            sample: Sample::Float,
            channels: 2,
        };
        Channels::Mono.append(
            &mut output,
            &floats(&[0.5, 0.0, 1.0, 1.0, -1.0, 0.0]),
            stereo,
        );
        assert_eq!(output, [8192, 32_767, -16_384]);

        output.clear();
        let mono = Layout {
            sample: Sample::Int16,
            channels: 1,
        };
        Channels::Mono.append(&mut output, &[1, 0, 0xFF, 0xFF], mono);
        assert_eq!(output, [1, -1]);
    }
}
