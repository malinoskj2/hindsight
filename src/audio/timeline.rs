//! Keeps the run of frames on the clock the video uses, checked against WASAPI's packet stamps.

use crate::replay::{CLOCK_RATE, samples_of, ticks_of};
use std::{
    num::NonZeroU32,
    ops::{Add, AddAssign, Neg, Sub},
};

/// Drift allowed before the run is corrected: 4 ms.
const TOLERANCE: Ticks = Ticks(CLOCK_RATE / 250);
/// A packet stamped this far ahead has a broken timestamp, since `catch_up` fills a real gap
/// first.
const IMPLAUSIBLE_GAP: Ticks = Ticks(5 * CLOCK_RATE);

/// A count or position of sample frames: one sample for each channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Frames(pub(super) u64);

impl Frames {
    pub(super) const ZERO: Self = Self(0);

    /// Returns how long this many frames last at `rate` frames a second, saturating.
    pub(super) fn to_ticks(self, rate: NonZeroU32) -> Ticks {
        Ticks(ticks_of(self.0, rate.get()))
    }

    /// Returns the whole frames at `rate` a second in `ticks`, rounded down, or none for a
    /// negative span.
    pub(super) fn from_ticks(ticks: Ticks, rate: NonZeroU32) -> Self {
        Self(u64::try_from(samples_of(ticks.0, rate.get())).unwrap_or(0))
    }
}

impl From<u32> for Frames {
    fn from(frames: u32) -> Self {
        Self(u64::from(frames))
    }
}

impl Add for Frames {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for Frames {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl Sub for Frames {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

/// A time or span on the shared clock, in 100 ns ticks (`CLOCK_RATE` a second).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Ticks(pub(super) i64);

impl Ticks {
    pub(super) const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub(super) const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl Neg for Ticks {
    type Output = Self;

    fn neg(self) -> Self {
        Self(self.0.saturating_neg())
    }
}

pub(super) struct Timeline {
    rate: NonZeroU32,
    /// Clock time of the first frame.
    anchor: Ticks,
    /// Frames placed so far, silence included.
    placed: Frames,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Placement {
    /// Frames of silence to put before the packet.
    pub(super) silence: Frames,
    /// Leading frames of the packet that overlap ones already placed, for the caller to drop.
    pub(super) overlap: Frames,
}

impl Timeline {
    pub(super) const fn new(rate: NonZeroU32, anchor: Ticks) -> Self {
        Self {
            rate,
            anchor,
            placed: Frames::ZERO,
        }
    }

    fn next_time(&self) -> Ticks {
        self.anchor.saturating_add(self.placed.to_ticks(self.rate))
    }

    /// Places a packet of `frames` frames stamped at `time`, if it was stamped.
    pub(super) fn place(&mut self, time: Option<Ticks>, frames: Frames) -> Placement {
        let offset = time.map_or(Ticks(0), |time| time.saturating_sub(self.next_time()));
        let placement = if offset > TOLERANCE && offset < IMPLAUSIBLE_GAP {
            Placement {
                silence: Frames::from_ticks(offset, self.rate),
                overlap: Frames::ZERO,
            }
        } else if offset < -TOLERANCE {
            Placement {
                silence: Frames::ZERO,
                overlap: Frames::from_ticks(-offset, self.rate).min(frames),
            }
        } else {
            Placement {
                silence: Frames::ZERO,
                overlap: Frames::ZERO,
            }
        };
        self.placed += placement.silence + (frames - placement.overlap);
        placement
    }

    /// Places and returns the frames of silence that bring the run up to `time`.
    pub(super) fn catch_up(&mut self, time: Ticks) -> Frames {
        let behind = time.saturating_sub(self.next_time());
        if behind <= Ticks(0) {
            return Frames::ZERO;
        }
        let silence = Frames::from_ticks(behind, self.rate);
        self.placed += silence;
        silence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: NonZeroU32 = NonZeroU32::new(48_000).unwrap();
    const TEN_MS: i64 = 100_000;

    #[test]
    fn packets_on_time_follow_each_other() {
        let mut timeline = Timeline::new(RATE, Ticks(1_000));
        for packet in 0..100 {
            let placement = timeline.place(Some(Ticks(1_000 + packet * TEN_MS)), Frames(480));
            assert_eq!(
                placement,
                Placement {
                    silence: Frames(0),
                    overlap: Frames(0)
                }
            );
        }
        assert_eq!(timeline.next_time(), Ticks(1_000 + 100 * TEN_MS));
    }

    #[test]
    fn jitter_within_the_tolerance_is_left_alone() {
        let mut timeline = Timeline::new(RATE, Ticks(0));
        timeline.place(Some(Ticks(0)), Frames(480));
        let placement = timeline.place(Some(Ticks(TEN_MS + TOLERANCE.0 - 1)), Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(0),
                overlap: Frames(0)
            }
        );
        let placement = timeline.place(Some(Ticks(2 * TEN_MS - TOLERANCE.0 + 1)), Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(0),
                overlap: Frames(0)
            }
        );
    }

    #[test]
    fn a_gap_is_filled_with_silence() {
        let mut timeline = Timeline::new(RATE, Ticks(0));
        timeline.place(Some(Ticks(0)), Frames(480));
        let placement = timeline.place(Some(Ticks(5 * TEN_MS)), Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(1920),
                overlap: Frames(0)
            }
        );
        assert_eq!(timeline.next_time(), Ticks(6 * TEN_MS));
    }

    #[test]
    fn a_fast_sound_card_has_its_overlap_dropped() {
        let mut timeline = Timeline::new(RATE, Ticks(0));
        timeline.place(Some(Ticks(0)), Frames(4800));
        let placement = timeline.place(Some(Ticks(10 * TEN_MS - TEN_MS / 2)), Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(0),
                overlap: Frames(240)
            }
        );
        assert_eq!(timeline.next_time(), Ticks(10 * TEN_MS + TEN_MS / 2));
    }

    #[test]
    fn drift_never_accumulates_past_the_tolerance() {
        let stamp = |samples: u64| i64::try_from(u128::from(samples) * 100_000_000 / 480_048);
        let mut timeline = Timeline::new(RATE, Ticks(0));
        let mut captured: u64 = 0;
        let mut corrections = 0;
        for _ in 0..60_000 {
            let placement = timeline.place(Some(Ticks(stamp(captured).unwrap())), Frames(480));
            corrections += u32::from(placement.overlap > Frames::ZERO);
            captured += 480;
            let error = timeline.next_time().0 - stamp(captured).unwrap();
            assert!(error.abs() <= TOLERANCE.0 + TEN_MS / 10, "{error}");
        }
        assert_eq!(corrections, 14);
    }

    #[test]
    fn silence_catches_the_run_up_and_broken_stamps_are_ignored() {
        let mut timeline = Timeline::new(RATE, Ticks(0));
        assert_eq!(timeline.catch_up(Ticks(-5)), Frames(0));
        assert_eq!(timeline.catch_up(Ticks(TEN_MS)), Frames(480));
        assert_eq!(timeline.catch_up(Ticks(TEN_MS)), Frames(0));

        let placement = timeline.place(Some(Ticks(IMPLAUSIBLE_GAP.0 * 2)), Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(0),
                overlap: Frames(0)
            }
        );
        let placement = timeline.place(None, Frames(480));
        assert_eq!(
            placement,
            Placement {
                silence: Frames(0),
                overlap: Frames(0)
            }
        );
        assert_eq!(timeline.next_time(), Ticks(3 * TEN_MS));
    }
}
