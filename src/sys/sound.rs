//! The tones that say whether a clip was saved. They are generated here at a low level, so they
//! are heard without taking over from the game, and they play on their own thread so a save
//! never waits on the sound card.

use std::f64::consts::TAU;
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use winsafe::{self as w, Snd};

const SAMPLE_RATE: u32 = 22_050;
/// About -14 dBFS: clear over a game, not startling.
const AMPLITUDE: f64 = 0.2;
const FADE_MS: u32 = 5;

/// Two rising tones: a clip was written.
const SAVED: &[(u32, u32)] = &[(880, 90), (0, 30), (1320, 120)];
/// Two falling tones: the save failed, or there was nothing to save.
const FAILED: &[(u32, u32)] = &[(330, 150), (0, 40), (220, 220)];

/// Plays the sound for a clip that was saved.
pub(crate) fn saved() {
    play(SAVED);
}

/// Plays the sound for a save that failed.
pub(crate) fn failed() {
    play(FAILED);
}

#[cfg(windows)]
fn play(tones: &'static [(u32, u32)]) {
    let wav = wav(tones);
    // A new tone cuts off one still playing; a failed spawn or playback only loses the sound.
    let _ = thread::Builder::new()
        .name("sound".to_owned())
        .spawn(move || {
            let _ = w::PlaySound(Snd::MemSync {
                src: &wav,
                default: false,
                stop: true,
                sentry: false,
            });
        });
}

#[cfg(not(windows))]
fn play(_tones: &'static [(u32, u32)]) {}

/// A mono 16-bit WAV of the `(frequency Hz, milliseconds)` tones, where a frequency of 0 is
/// silence. Each tone fades in and out so it does not click.
fn wav(tones: &[(u32, u32)]) -> Vec<u8> {
    let data = pcm(tones);
    let data_len = u32::try_from(data.len()).expect("a tone is a few hundred milliseconds");
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

/// The tones' samples one after another, as little-endian 16-bit PCM.
fn pcm(tones: &[(u32, u32)]) -> Vec<u8> {
    let mut data = Vec::new();
    for &(frequency, milliseconds) in tones {
        let count = SAMPLE_RATE * milliseconds / 1000;
        for index in 0..count {
            data.extend_from_slice(&sample(frequency, index, count).to_le_bytes());
        }
    }
    data
}

/// Sample `index` of a tone `count` samples long, faded in and out over `FADE_MS`; a frequency
/// of 0 is silence.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the scaled sample is within the i16 range, and the rounding is intended"
)]
fn sample(frequency: u32, index: u32, count: u32) -> i16 {
    if frequency == 0 {
        return 0;
    }
    let fade = SAMPLE_RATE * FADE_MS / 1000;
    let time = f64::from(index) / f64::from(SAMPLE_RATE);
    let edge = index.min(count - 1 - index);
    let envelope = (f64::from(edge) / f64::from(fade)).min(1.0);
    let value = envelope * AMPLITUDE * (TAU * f64::from(frequency) * time).sin();
    (value * f64::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_are_a_complete_wav_within_the_quiet_level() {
        let wav = wav(SAVED);
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + data_len as usize);
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            36 + data_len
        );

        let peak = samples(&wav)
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 1_000, "the tones are audible");
        assert!(
            f64::from(peak) <= AMPLITUDE * f64::from(i16::MAX) + 1.0,
            "the tones stay quiet"
        );
    }

    #[test]
    fn silence_is_zeros_and_each_tone_fades_in() {
        let wav = wav(&[(0, 10), (440, 10)]);
        let samples = samples(&wav);
        let silence = (SAMPLE_RATE * 10 / 1000) as usize;
        assert!(samples[..silence].iter().all(|&sample| sample == 0));
        assert_eq!(samples[silence], 0, "the tone starts from silence");
    }

    /// The 16-bit samples after the 44-byte header.
    fn samples(wav: &[u8]) -> Vec<i16> {
        let (pairs, _) = wav[44..].as_chunks::<2>();
        pairs.iter().map(|&pair| i16::from_le_bytes(pair)).collect()
    }
}
