//! Test helpers: AAC-LC frames in ADTS (48 kHz, stereo, no checksum), and decoding with ffmpeg.

use std::{fs, path::Path, process::Command, sync::Arc};

const HEADER_BYTES: usize = 7;

pub(crate) fn adts(frames: &[Arc<[u8]>]) -> Vec<u8> {
    let mut stream = Vec::new();
    for frame in frames {
        let [high, low] = u16::try_from(frame.len() + HEADER_BYTES)
            .unwrap()
            .to_be_bytes();
        stream.extend([
            0xFF,
            0xF1,
            0x4C,
            0x80 | (high >> 3),
            (high << 5) | (low >> 3),
            (low << 5) | 0x1F,
            0xFC,
        ]);
        stream.extend_from_slice(frame);
    }
    stream
}

pub(crate) fn frames(mut stream: &[u8]) -> Vec<Arc<[u8]>> {
    let mut frames = Vec::new();
    while !stream.is_empty() {
        assert_eq!(
            &stream[..2],
            &[0xFF, 0xF1],
            "an ADTS header without a checksum"
        );
        let length = (usize::from(stream[3] & 0x03) << 11)
            | (usize::from(stream[4]) << 3)
            | usize::from(stream[5] >> 5);
        frames.push(Arc::from(&stream[HEADER_BYTES..length]));
        stream = &stream[length..];
    }
    frames
}

/// The sound of `input` as mono 16-bit samples, decoded by the ffmpeg on the path.
#[cfg_attr(all(not(windows), test), expect(dead_code))]
pub(crate) fn decode_mono(input: &Path) -> Vec<i16> {
    let raw = input.with_extension("raw");
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(input)
        .args(["-map", "0:a", "-f", "s16le", "-ac", "1"])
        .arg(&raw)
        .status()
        .unwrap();
    assert!(status.success());
    fs::read(&raw)
        .unwrap()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect()
}

/// The position of the loudest sample.
#[cfg_attr(all(not(windows), test), expect(dead_code))]
pub(crate) fn peak(samples: &[i16]) -> usize {
    (0..samples.len())
        .max_by_key(|&index| samples[index].unsigned_abs())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_framing_round_trips() {
        let frames: Vec<Arc<[u8]>> = vec![Arc::from(vec![1; 300]), Arc::from(vec![2; 9])];
        let stream = adts(&frames);
        assert_eq!(stream.len(), 300 + 9 + 2 * HEADER_BYTES);
        assert_eq!(stream[..3], [0xFF, 0xF1, 0x4C]);
        assert_eq!(super::frames(&stream), frames);
    }
}
