//! Converts WASAPI's float or 16-bit samples to the interleaved 16-bit stereo the encoder takes.

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

/// Appends `data` to `output` as interleaved stereo. Mono is doubled. `WaveFormat::layout` only
/// accepts one- or two-channel formats; any channels past two would be dropped.
pub(super) fn append_stereo(output: &mut Vec<i16>, data: &[u8], layout: Layout) {
    let channels = usize::from(layout.channels);
    match layout.sample {
        Sample::Float => append(output, data.as_chunks::<4>().0, channels, |bytes| {
            from_float(f32::from_le_bytes(bytes))
        }),
        Sample::Int16 => append(
            output,
            data.as_chunks::<2>().0,
            channels,
            i16::from_le_bytes,
        ),
    }
}

fn append<const N: usize>(
    output: &mut Vec<i16>,
    samples: &[[u8; N]],
    channels: usize,
    convert: impl Fn([u8; N]) -> i16,
) {
    output.reserve(samples.len() / channels * 2);
    for frame in samples.chunks_exact(channels) {
        let left = convert(frame[0]);
        let right = frame.get(1).map_or(left, |&right| convert(right));
        output.extend([left, right]);
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
        append_stereo(
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
        append_stereo(&mut output, &[1, 0, 0xFF, 0xFF], mono);
        assert_eq!(output, [1, 1, -1, -1]);

        output.clear();
        let surround = Layout {
            sample: Sample::Int16,
            channels: 3,
        };
        append_stereo(&mut output, &[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0], surround);
        assert_eq!(output, [1, 2, 4, 5]);
    }
}
