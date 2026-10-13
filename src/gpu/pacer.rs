//! The frame clock: a high-resolution waitable timer the capture thread sleeps on between frames.

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

const HUNDRED_NANOSECONDS: u128 = 100;

/// A duration in the 100 ns units of `FILETIME` and `TimeSpan`.
pub(super) fn filetime_units(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / HUNDRED_NANOSECONDS).unwrap_or(i64::MAX)
}

pub(super) struct Timer(Owned<HANDLE>);

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

pub(super) struct Pacer {
    timer: Timer,
    start: Instant,
    period: Duration,
    next: u64,
}

impl Pacer {
    pub(super) fn new(frame_rate: u32) -> windows::core::Result<Self> {
        Ok(Self {
            timer: Timer::new()?,
            start: Instant::now(),
            period: Duration::from_secs(1) / frame_rate,
            next: 0,
        })
    }

    pub(super) const fn period(&self) -> Duration {
        self.period
    }

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
        self.timer.sleep(due.saturating_sub(elapsed))?;
        let index = self.next;
        self.next += 1;
        Ok(index)
    }
}

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

    #[test]
    fn a_thread_that_fell_behind_skips_the_missed_ticks() {
        let mut pacer = Pacer::new(240).unwrap();
        let first = pacer.wait().unwrap();
        std::thread::sleep(pacer.period() * 4);
        let late = pacer.wait().unwrap();
        assert!(late >= first + 4, "{late} after {first}");
        assert!(pacer.wait().unwrap() > late);
    }
}
