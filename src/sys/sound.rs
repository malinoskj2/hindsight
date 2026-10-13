//! The tones that say how a save is going, played on a thread of their own.

use std::{
    f64::consts::TAU,
    io::Cursor,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};
#[cfg(windows)]
use winsafe::{self as w, Snd};

const SAMPLE_RATE: u32 = 22_050;
const AMPLITUDE: f64 = 0.2;
const FADE_MILLISECONDS: u32 = 5;

const PROGRESS: &[(u32, u32)] = &[(990, 35)];
const SAVED: &[(u32, u32)] = &[(880, 90), (0, 30), (1320, 120)];
const FAILED: &[(u32, u32)] = &[(440, 120), (0, 30), (330, 120), (0, 30), (220, 300)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Progress,
    Saved,
    Failed,
}

impl Tone {
    /// `(frequency Hz, milliseconds)` pairs; a frequency of 0 is silence.
    fn tones(self) -> &'static [(u32, u32)] {
        match self {
            Self::Progress => PROGRESS,
            Self::Saved => SAVED,
            Self::Failed => FAILED,
        }
    }
}

/// The thread ends with its last `Sender`. A tone still playing finishes; of those asked for
/// meanwhile only the latest plays.
pub(crate) fn start() -> Sender<Tone> {
    let (sender, receiver) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("sound".to_owned())
        .spawn(move || serve(&receiver));
    sender
}

fn serve(tones: &Receiver<Tone>) {
    while let Ok(first) = tones.recv() {
        play(tones.try_iter().last().unwrap_or(first));
    }
}

#[cfg(windows)]
fn play(tone: Tone) {
    let _ = w::PlaySound(Snd::MemSync {
        src: &wav(tone.tones()),
        default: false,
        stop: true,
        sentry: false,
    });
}

#[cfg(not(windows))]
fn play(_tone: Tone) {}

fn wav(tones: &[(u32, u32)]) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut wav, spec).expect("a WAV header fits in memory");
    for &(frequency, milliseconds) in tones {
        let count = SAMPLE_RATE * milliseconds / 1000;
        for index in 0..count {
            writer
                .write_sample(sample(frequency, index, count))
                .expect("a sample fits in memory");
        }
    }
    writer.finalize().expect("the WAV lengths fit in memory");
    wav.into_inner()
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the scaled sample is within the i16 range, and the rounding is intended"
)]
fn sample(frequency: u32, index: u32, count: u32) -> i16 {
    if frequency == 0 {
        return 0;
    }
    let fade = SAMPLE_RATE * FADE_MILLISECONDS / 1000;
    let time = f64::from(index) / f64::from(SAMPLE_RATE);
    let edge = index.min(count - 1 - index);
    let envelope = (f64::from(edge) / f64::from(fade)).min(1.0);
    let value = envelope * AMPLITUDE * (TAU * f64::from(frequency) * time).sin();
    (value * f64::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(wav: &[u8]) -> Vec<i16> {
        let (pairs, _) = wav[44..].as_chunks::<2>();
        pairs.iter().map(|&pair| i16::from_le_bytes(pair)).collect()
    }

    #[test]
    fn tones_are_a_complete_wav_within_the_quiet_level() {
        let wav = wav(SAVED);
        let data_length = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + data_length as usize);
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            36 + data_length
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

    fn length(tones: &[(u32, u32)]) -> u32 {
        tones.iter().map(|&(_, milliseconds)| milliseconds).sum()
    }

    fn pitches(tones: &[(u32, u32)]) -> Vec<u32> {
        tones
            .iter()
            .map(|&(frequency, _)| frequency)
            .filter(|&frequency| frequency > 0)
            .collect()
    }

    #[test]
    fn the_three_tones_are_told_apart_by_ear() {
        let (progress, saved, failed) = (
            Tone::Progress.tones(),
            Tone::Saved.tones(),
            Tone::Failed.tones(),
        );
        assert_eq!(pitches(progress).len(), 1, "progress is a single blip");
        assert!(
            length(progress) <= 50,
            "a blip delays the outcome very little"
        );
        assert!(pitches(saved).is_sorted(), "saved rises");
        assert!(pitches(failed).is_sorted_by(|a, b| a > b), "failed falls");
        assert!(pitches(failed).len() > pitches(saved).len());
        assert!(length(failed) > length(saved), "failed lasts longer");
        assert!(pitches(failed).last() < pitches(progress).first());
        for tone in [Tone::Progress, Tone::Saved, Tone::Failed] {
            assert!(wav(tone.tones()).len() > 44, "{tone:?} has samples");
        }
    }

    #[test]
    fn silence_is_zeros_and_each_tone_fades_in() {
        let wav = wav(&[(0, 10), (440, 10)]);
        let samples = samples(&wav);
        let silence = (SAMPLE_RATE * 10 / 1000) as usize;
        assert!(samples[..silence].iter().all(|&sample| sample == 0));
        assert_eq!(samples[silence], 0, "the tone starts from silence");
    }
}
