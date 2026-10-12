//! The performance counter, in the 100 ns units WASAPI stamps its packets in.

use crate::replay::CLOCK_RATE;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub(crate) fn now() -> i64 {
    let (mut counter, mut frequency) = (0, 0);
    // SAFETY: both calls write one integer to a live local; a failure leaves the zeros.
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
