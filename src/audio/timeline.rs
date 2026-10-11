//! Keeps the captured sound on the clock the video uses. The encoder takes one unbroken run of
//! samples, so sample `n` plays `n / rate` seconds after the first; this checks that against
//! the performance-counter time WASAPI stamps on every packet and corrects the run when the two
//! part: silence where nothing was captured, dropped samples where the sound card's clock ran
//! ahead. The replay module's `audio` describes the clock both streams share.

use crate::replay::CLOCK_RATE;

/// How far the samples may stray from the times stamped on them before the run is corrected,
/// in clock ticks: 4 ms, less than a frame at 240 fps. Sound cards drift some tens of parts per
/// million from the counter, so a correction is due every few minutes at most, and the small
/// discontinuity it makes is rarely heard.
pub(crate) const TOLERANCE: i64 = CLOCK_RATE / 250;
/// A packet stamped this far ahead of the run, in clock ticks, has a broken timestamp: a real
/// gap is filled by [`Timeline::catch_up`] long before it grows this large.
const IMPLAUSIBLE_GAP: i64 = 5 * CLOCK_RATE;

/// Where the run of samples stands on the clock.
pub(crate) struct Timeline {
    rate: u32,
    /// The clock time of the first sample.
    anchor: i64,
    /// Samples placed so far, silence included.
    placed: u64,
}

/// What fitting a packet onto the run takes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Samples of silence to put in front of the packet.
    pub(crate) silence: u64,
    /// Samples at the start of the packet to drop, which overlap ones already placed.
    pub(crate) skip: u32,
}

impl Timeline {
    /// A run at `rate` samples a second whose first sample is due at clock time `anchor`.
    pub(crate) const fn new(rate: u32, anchor: i64) -> Self {
        Self {
            rate,
            anchor,
            placed: 0,
        }
    }

    /// The clock time the next sample is due at.
    pub(crate) fn next_time(&self) -> i64 {
        let offset = i128::from(self.placed) * i128::from(CLOCK_RATE) / i128::from(self.rate);
        self.anchor
            .saturating_add(i64::try_from(offset).unwrap_or(i64::MAX))
    }

    /// Places a packet of `frames` samples whose first was captured at clock time `time`, or
    /// right after the previous one when its time is unknown or implausible.
    pub(crate) fn place(&mut self, time: Option<i64>, frames: u32) -> Placement {
        let offset = time.map_or(0, |time| time.saturating_sub(self.next_time()));
        let placement = if offset > TOLERANCE && offset < IMPLAUSIBLE_GAP {
            Placement {
                silence: self.samples_in(offset),
                skip: 0,
            }
        } else if offset < -TOLERANCE {
            let overlap = self.samples_in(offset.saturating_neg());
            Placement {
                silence: 0,
                skip: u32::try_from(overlap).unwrap_or(u32::MAX).min(frames),
            }
        } else {
            Placement {
                silence: 0,
                skip: 0,
            }
        };
        self.placed += placement.silence + u64::from(frames - placement.skip);
        placement
    }

    /// Advances the run to clock time `time` when nothing has arrived to fill it, and returns
    /// the samples of silence that takes; none when the run is already there.
    pub(crate) fn catch_up(&mut self, time: i64) -> u64 {
        let behind = time.saturating_sub(self.next_time());
        if behind <= 0 {
            return 0;
        }
        let silence = self.samples_in(behind);
        self.placed += silence;
        silence
    }

    /// The whole samples in `ticks` of the clock.
    fn samples_in(&self, ticks: i64) -> u64 {
        let samples = i128::from(ticks) * i128::from(self.rate) / i128::from(CLOCK_RATE);
        u64::try_from(samples).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 48 kHz: 480 samples are 10 ms, 100 000 ticks.
    const RATE: u32 = 48_000;
    const TEN_MS: i64 = 100_000;

    #[test]
    fn packets_on_time_follow_each_other() {
        let mut timeline = Timeline::new(RATE, 1_000);
        for packet in 0..100 {
            let placement = timeline.place(Some(1_000 + packet * TEN_MS), 480);
            assert_eq!(
                placement,
                Placement {
                    silence: 0,
                    skip: 0
                }
            );
        }
        assert_eq!(timeline.next_time(), 1_000 + 100 * TEN_MS);
    }

    #[test]
    fn jitter_within_the_tolerance_is_left_alone() {
        let mut timeline = Timeline::new(RATE, 0);
        timeline.place(Some(0), 480);
        let placement = timeline.place(Some(TEN_MS + TOLERANCE - 1), 480);
        assert_eq!(
            placement,
            Placement {
                silence: 0,
                skip: 0
            }
        );
        let placement = timeline.place(Some(2 * TEN_MS - TOLERANCE + 1), 480);
        assert_eq!(
            placement,
            Placement {
                silence: 0,
                skip: 0
            }
        );
    }

    #[test]
    fn a_gap_is_filled_with_silence() {
        let mut timeline = Timeline::new(RATE, 0);
        timeline.place(Some(0), 480);
        // The next packet comes 50 ms late: 40 ms, 1920 samples, were lost.
        let placement = timeline.place(Some(5 * TEN_MS), 480);
        assert_eq!(
            placement,
            Placement {
                silence: 1920,
                skip: 0
            }
        );
        assert_eq!(timeline.next_time(), 6 * TEN_MS);
    }

    #[test]
    fn a_fast_sound_card_has_its_overlap_dropped() {
        let mut timeline = Timeline::new(RATE, 0);
        timeline.place(Some(0), 4800);
        // Stamped 5 ms before the run's end: the first 240 samples were already placed.
        let placement = timeline.place(Some(10 * TEN_MS - TEN_MS / 2), 480);
        assert_eq!(
            placement,
            Placement {
                silence: 0,
                skip: 240
            }
        );
        assert_eq!(timeline.next_time(), 10 * TEN_MS + TEN_MS / 2);
    }

    #[test]
    fn drift_never_accumulates_past_the_tolerance() {
        // A sound card 100 parts per million fast: 48 004.8 samples a second of counter time.
        let stamp = |samples: u64| i64::try_from(u128::from(samples) * 100_000_000 / 480_048);
        let mut timeline = Timeline::new(RATE, 0);
        let mut captured: u64 = 0;
        let mut corrections = 0;
        // Ten minutes of 10 ms packets, each stamped with the counter time of its first sample.
        for _ in 0..60_000 {
            let placement = timeline.place(Some(stamp(captured).unwrap()), 480);
            corrections += u32::from(placement.skip > 0);
            captured += 480;
            let error = timeline.next_time() - stamp(captured).unwrap();
            assert!(error.abs() <= TOLERANCE + TEN_MS / 10, "{error}");
        }
        // 60 ms of drift, corrected 4 ms at a time.
        assert_eq!(corrections, 14);
    }

    #[test]
    fn silence_catches_the_run_up_and_broken_stamps_are_ignored() {
        let mut timeline = Timeline::new(RATE, 0);
        assert_eq!(timeline.catch_up(-5), 0);
        assert_eq!(timeline.catch_up(TEN_MS), 480);
        assert_eq!(timeline.catch_up(TEN_MS), 0);

        let placement = timeline.place(Some(IMPLAUSIBLE_GAP * 2), 480);
        assert_eq!(
            placement,
            Placement {
                silence: 0,
                skip: 0
            }
        );
        let placement = timeline.place(None, 480);
        assert_eq!(
            placement,
            Placement {
                silence: 0,
                skip: 0
            }
        );
        assert_eq!(timeline.next_time(), 3 * TEN_MS);
    }
}
