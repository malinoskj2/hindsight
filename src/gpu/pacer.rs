//! The frame clock: a high-resolution waitable timer the capture thread sleeps on between frames.

use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0},
        System::Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE,
            SetWaitableTimer, TIMER_ALL_ACCESS, WaitForSingleObject,
        },
    },
    core::{Owned, PCWSTR},
};

const HUNDRED_NANOSECONDS: u128 = 100;

/// A duration in the 100 ns units of `FILETIME` and `TimeSpan`.
pub(super) fn filetime_units(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / HUNDRED_NANOSECONDS).unwrap_or(i64::MAX)
}

pub(super) struct Timer(Owned<HANDLE>);

#[expect(unsafe_code, reason = "calls the waitable timer API")]
impl Timer {
    pub(super) fn new() -> windows::core::Result<Self> {
        // SAFETY: an unnamed timer with default security.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }?;
        // SAFETY: the timer was just created, and only this `Owned` closes it.
        Ok(Self(unsafe { Owned::new(timer) }))
    }

    pub(super) fn sleep(&self, duration: Duration) -> windows::core::Result<()> {
        // Relative due times are negative.
        let relative = -filetime_units(duration);
        // SAFETY: open timer; one-shot, no completion routine; `relative` outlives the call.
        unsafe { SetWaitableTimer(*self.0, &raw const relative, 0, None, None, false) }?;
        // SAFETY: waits on the open timer.
        if unsafe { WaitForSingleObject(*self.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok(())
    }
}

/// A tick of the frame clock, counted from the start of the session, identifying the frame
/// shown at that tick. NVENC carries it through a frame's encode as the picture's timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FrameIndex(u64);

impl FrameIndex {
    pub(super) const fn new(index: u64) -> Self {
        Self(index)
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }
}

/// The source of time the pacer reads and sleeps on. A fake clock makes the pacer's tests exact.
pub(super) trait Clock {
    /// Time since the clock started.
    fn elapsed(&self) -> Duration;

    fn sleep(&mut self, duration: Duration) -> windows::core::Result<()>;
}

/// The wall clock, which sleeps through the high-resolution timer.
pub(super) struct WallClock {
    timer: Timer,
    start: Instant,
}

impl WallClock {
    fn new() -> windows::core::Result<Self> {
        Ok(Self {
            timer: Timer::new()?,
            start: Instant::now(),
        })
    }
}

impl Clock for WallClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&mut self, duration: Duration) -> windows::core::Result<()> {
        self.timer.sleep(duration)
    }
}

pub(super) struct Pacer<C = WallClock> {
    clock: C,
    period: Duration,
    next: u64,
}

impl Pacer {
    pub(super) fn new(frame_rate: NonZeroU32) -> windows::core::Result<Self> {
        Ok(Self::with_clock(WallClock::new()?, frame_rate))
    }
}

impl<C: Clock> Pacer<C> {
    fn with_clock(clock: C, frame_rate: NonZeroU32) -> Self {
        Self {
            clock,
            period: Duration::from_secs(1) / frame_rate.get(),
            next: 0,
        }
    }

    pub(super) const fn period(&self) -> Duration {
        self.period
    }

    pub(super) fn wait(&mut self) -> windows::core::Result<FrameIndex> {
        let elapsed = self.clock.elapsed();
        let current = tick(elapsed, self.period);
        if current >= self.next {
            self.next = current + 1;
            return Ok(FrameIndex(current));
        }

        let due = Duration::from_nanos(
            u64::try_from(self.period.as_nanos() * u128::from(self.next)).unwrap_or(u64::MAX),
        );
        self.clock.sleep(due.saturating_sub(elapsed))?;
        let index = self.next;
        self.next += 1;
        Ok(FrameIndex(index))
    }
}

fn tick(elapsed: Duration, period: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos() / period.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Time advances only when the pacer sleeps or a test advances it.
    #[derive(Default)]
    struct FakeClock {
        now: Duration,
    }

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.now
        }

        fn sleep(&mut self, duration: Duration) -> windows::core::Result<()> {
            self.now += duration;
            Ok(())
        }
    }

    fn pacer() -> Pacer<FakeClock> {
        Pacer::with_clock(FakeClock::default(), NonZeroU32::new(240).unwrap())
    }

    #[test]
    fn ticks_count_whole_periods() {
        let period = Duration::from_secs(1) / 60;
        assert_eq!(tick(Duration::ZERO, period), 0);
        assert_eq!(
            tick(period.saturating_sub(Duration::from_nanos(1)), period),
            0
        );
        assert_eq!(tick(period * 90, period), 90);
    }

    #[test]
    fn paces_without_returning_a_tick_twice() {
        let mut pacer = pacer();
        let ticks: Vec<u64> = (0..24).map(|_| pacer.wait().unwrap().get()).collect();

        assert_eq!(ticks, (0..24).collect::<Vec<u64>>());
        assert_eq!(pacer.clock.elapsed(), pacer.period() * 23);
    }

    #[test]
    fn a_tick_is_returned_once_its_period_has_begun() {
        let mut pacer = pacer();
        assert_eq!(pacer.wait().unwrap(), FrameIndex(0));
        pacer.clock.now = pacer.period() / 2;
        assert_eq!(pacer.wait().unwrap(), FrameIndex(1));
        assert_eq!(pacer.clock.elapsed(), pacer.period());
    }

    #[test]
    fn a_thread_that_fell_behind_skips_the_missed_ticks() {
        let mut pacer = pacer();
        let first = pacer.wait().unwrap();
        pacer.clock.now += pacer.period() * 4;
        let late = pacer.wait().unwrap();
        assert_eq!((first, late), (FrameIndex(0), FrameIndex(4)));
        let before = pacer.clock.elapsed();
        assert_eq!(pacer.wait().unwrap(), FrameIndex(5));
        assert_eq!(pacer.clock.elapsed(), before + pacer.period());
    }
}
