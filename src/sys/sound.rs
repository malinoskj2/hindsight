//! The tones that say how a save is going: a short blip each second while a clip is written,
//! then whether it was saved. They are generated here at a low level, so they are heard without
//! taking over from the game, and they play on their own thread so a save never waits on the
//! sound card.

use std::{f64::consts::TAU, io::Cursor};
#[cfg(windows)]
use std::{
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};
#[cfg(windows)]
use winsafe::{self as w, Snd};

/// Samples per second; plenty for a few short tones.
const SAMPLE_RATE: u32 = 22_050;
/// About -14 dBFS: clear over a game, not startling.
const AMPLITUDE: f64 = 0.2;
/// How long each tone takes to fade in and out.
const FADE_MILLISECONDS: u32 = 5;

/// One short blip between the other two in pitch: a clip is being written.
const PROGRESS: &[(u32, u32)] = &[(990, 35)];
/// Two rising tones: a clip was written.
const SAVED: &[(u32, u32)] = &[(880, 90), (0, 30), (1320, 120)];
/// Three falling tones, low and ending long: the save failed, or there was nothing to save.
const FAILED: &[(u32, u32)] = &[(440, 120), (0, 30), (330, 120), (0, 30), (220, 300)];

/// What a tone says about a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// A clip is still being written.
    Progress,
    /// The clip was saved.
    Saved,
    /// The save failed, or there was nothing to save.
    Failed,
}

impl Tone {
    /// The `(frequency Hz, milliseconds)` tones that make up the sound.
    fn tones(self) -> &'static [(u32, u32)] {
        match self {
            Self::Progress => PROGRESS,
            Self::Saved => SAVED,
            Self::Failed => FAILED,
        }
    }
}

/// The number of the latest tone asked for; a tone that is no longer the latest when its turn
/// comes is dropped.
#[cfg(windows)]
static REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Held while a tone plays, so tones play one at a time in the order they were asked for.
#[cfg(windows)]
static PLAYING: Mutex<()> = Mutex::new(());

/// Plays `tone` on a thread of its own. A tone still playing is let finish, so a progress blip
/// never cuts off the outcome that follows it; of the tones asked for meanwhile only the latest
/// plays. A failed spawn or playback only loses the sound.
#[cfg(windows)]
pub(crate) fn play(tone: Tone) {
    let turn = REQUESTED.fetch_add(1, Ordering::Relaxed) + 1;
    let wav = wav(tone.tones());
    let _ = thread::Builder::new()
        .name("sound".to_owned())
        .spawn(move || {
            let _playing = PLAYING.lock().unwrap_or_else(PoisonError::into_inner);
            if REQUESTED.load(Ordering::Relaxed) != turn {
                return;
            }
            let _ = w::PlaySound(Snd::MemSync {
                src: &wav,
                default: false,
                stop: true,
                sentry: false,
            });
        });
}

/// Plays `tone`: there is no sound card to play it on outside Windows.
#[cfg(not(windows))]
pub(crate) fn play(_tone: Tone) {}

/// A mono 16-bit WAV of the `(frequency Hz, milliseconds)` tones, where a frequency of 0 is
/// silence. Each tone fades in and out so it does not click.
fn wav(tones: &[(u32, u32)]) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = Cursor::new(Vec::new());
    // Writing into memory cannot fail.
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

/// Sample `index` of a tone `count` samples long, faded in and out over `FADE_MILLISECONDS`; a
/// frequency of 0 is silence.
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

    /// The 16-bit samples after the 44-byte header.
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

    /// The milliseconds of sound in `tones`, silences included.
    fn length(tones: &[(u32, u32)]) -> u32 {
        tones.iter().map(|&(_, milliseconds)| milliseconds).sum()
    }

    /// The pitches of `tones`, silences left out.
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
