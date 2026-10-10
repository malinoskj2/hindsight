//! The frame clock: a high-resolution waitable timer that wakes the capture thread once per
//! output frame. The thread sleeps in the kernel in between; nothing spins.

use std::time::{Duration, Instant};
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

/// Nanoseconds in the 100 ns unit Windows measures timer due times and capture intervals in.
pub(super) const HUNDRED_NANOSECONDS: u128 = 100;

/// Ticks `frame_rate` times a second, counted from its creation.
pub(super) struct Pacer {
    timer: Owned<HANDLE>,
    start: Instant,
    period: Duration,
    /// The tick the next call returns, unless the thread has fallen behind it.
    next: u64,
}

impl Pacer {
    pub(super) fn new(frame_rate: u32) -> windows::core::Result<Self> {
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
        let timer = unsafe { Owned::new(timer) };
        Ok(Self {
            timer,
            start: Instant::now(),
            period: Duration::from_secs(1) / frame_rate,
            next: 0,
        })
    }

    pub(super) const fn period(&self) -> Duration {
        self.period
    }

    /// Sleeps until the next tick and returns its number. A thread that fell more than a frame
    /// behind gets the current tick at once: missed ticks are skipped, not caught up in a burst.
    pub(super) fn wait(&mut self) -> windows::core::Result<u64> {
        let elapsed = self.start.elapsed();
        let current = tick(elapsed, self.period);
        if current >= self.next {
            self.next = current + 1;
            return Ok(current);
        }

        let due = Duration::from_nanos(
            u64::try_from(self.period.as_nanos() * u128::from(self.next)).unwrap_or(u64::MAX),
        );
        self.sleep(due.saturating_sub(elapsed))?;
        let index = self.next;
        self.next += 1;
        Ok(index)
    }

    /// Arms the timer to fire once after `duration` and sleeps until it does.
    fn sleep(&self, duration: Duration) -> windows::core::Result<()> {
        // Relative due times are negative, in 100 ns units.
        let relative =
            -i64::try_from(duration.as_nanos() / HUNDRED_NANOSECONDS).unwrap_or(i64::MAX);
        // SAFETY: the timer is open; `relative` outlives the call; a one-shot timer (period 0)
        // with no completion routine.
        unsafe { SetWaitableTimer(*self.timer, &raw const relative, 0, None, None, false) }?;
        // SAFETY: waits on the open timer.
        if unsafe { WaitForSingleObject(*self.timer, INFINITE) } != WAIT_OBJECT_0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok(())
    }
}

/// The tick that `elapsed` falls in: the number of whole periods it spans.
fn tick(elapsed: Duration, period: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos() / period.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut pacer = Pacer::new(240).unwrap();
        let started = Instant::now();
        let ticks: Vec<u64> = (0..24).map(|_| pacer.wait().unwrap()).collect();

        assert!(ticks.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(started.elapsed() >= pacer.period() * 22);
    }
}
