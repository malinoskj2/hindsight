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
    #[cfg_attr(all(not(windows), test), expect(dead_code))]
    pub(super) const fn frame_bytes(self) -> usize {
        self.sample.bytes() * self.channels as usize
    }
}

/// Mono is doubled; channels past the first two are dropped.
pub(super) fn append_stereo(output: &mut Vec<i16>, data: &[u8], layout: Layout) {
    let channels = usize::from(layout.channels);
    let width = layout.sample.bytes();
    for frame in data.chunks_exact(width * channels) {
        let left = read(&frame[..width], layout.sample);
        let right = if channels > 1 {
            read(&frame[width..2 * width], layout.sample)
        } else {
            left
        };
        output.extend([left, right]);
    }
}

fn read(bytes: &[u8], sample: Sample) -> i16 {
    match sample {
        Sample::Float => {
            let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            // NaN survives the clamp and casts to 0.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "clamped to the i16 range first"
            )]
            let scaled = (value * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
            scaled
        }
        Sample::Int16 => i16::from_le_bytes([bytes[0], bytes[1]]),
    }
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
