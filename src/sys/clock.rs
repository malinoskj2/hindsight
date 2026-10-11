//! The clock the video frames and the sound are both placed on: the performance counter, in
//! the 100 ns units WASAPI stamps its packets in.

use crate::replay::CLOCK_RATE;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The performance counter now, in 100 ns units.
pub(crate) fn now() -> i64 {
    let (mut counter, mut frequency) = (0, 0);
    // SAFETY: both calls write one integer to a live local; they cannot fail on Windows XP or
    // newer, and a failure would leave the zeros, which read as the counter's start.
    unsafe {
        let _ = QueryPerformanceCounter(&raw mut counter);
        let _ = QueryPerformanceFrequency(&raw mut frequency);
    }
    if frequency <= 0 {
        return 0;
    }
    let ticks = i128::from(counter) * i128::from(CLOCK_RATE) / i128::from(frequency);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}
