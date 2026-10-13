//! Keeps the run of samples on the clock the video uses, checked against WASAPI's packet stamps.

use crate::replay::{CLOCK_RATE, samples_of, ticks_of};

/// Drift allowed before the run is corrected: 4 ms.
const TOLERANCE: i64 = CLOCK_RATE / 250;
/// A packet stamped this far ahead has a broken timestamp; `catch_up` fills a real gap first.
const IMPLAUSIBLE_GAP: i64 = 5 * CLOCK_RATE;

pub(super) struct Timeline {
    rate: u32,
    /// Clock time of the first sample.
    anchor: i64,
    /// Samples placed so far, silence included.
    placed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) silence: u64,
    /// Leading samples that overlap ones already placed.
    pub(super) skip: u32,
}

impl Timeline {
    pub(super) const fn new(rate: u32, anchor: i64) -> Self {
        Self {
            rate,
            anchor,
            placed: 0,
        }
    }

    fn next_time(&self) -> i64 {
        self.anchor.saturating_add(ticks_of(self.placed, self.rate))
    }

    pub(super) fn place(&mut self, time: Option<i64>, frames: u32) -> Placement {
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

    pub(super) fn catch_up(&mut self, time: i64) -> u64 {
        let behind = time.saturating_sub(self.next_time());
        if behind <= 0 {
            return 0;
        }
        let silence = self.samples_in(behind);
        self.placed += silence;
        silence
    }

    fn samples_in(&self, ticks: i64) -> u64 {
        u64::try_from(samples_of(ticks, self.rate)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let stamp = |samples: u64| i64::try_from(u128::from(samples) * 100_000_000 / 480_048);
        let mut timeline = Timeline::new(RATE, 0);
        let mut captured: u64 = 0;
        let mut corrections = 0;
        for _ in 0..60_000 {
            let placement = timeline.place(Some(stamp(captured).unwrap()), 480);
            corrections += u32::from(placement.skip > 0);
            captured += 480;
            let error = timeline.next_time() - stamp(captured).unwrap();
            assert!(error.abs() <= TOLERANCE + TEN_MS / 10, "{error}");
        }
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
