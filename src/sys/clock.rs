//! The performance counter, in the 100 ns units WASAPI stamps its packets in.

use crate::replay::CLOCK_RATE;

pub(crate) fn now() -> i64 {
    // A failure reads as zero; a zero frequency is checked below.
    let counter = winsafe::QueryPerformanceCounter().unwrap_or(0);
    let frequency = winsafe::QueryPerformanceFrequency().unwrap_or(0);
    if frequency <= 0 {
        return 0;
    }
    let ticks = i128::from(counter) * i128::from(CLOCK_RATE) / i128::from(frequency);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}
