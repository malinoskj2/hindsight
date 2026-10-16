//! Small helpers for the fixed-size fields of Win32 structures.

/// `size_of::<T>()` as the `u32` byte count a Win32 structure or call carries, checked at
/// compile time to fit.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the size is asserted to fit 32 bits at compile time"
)]
pub(crate) const fn cb_size<T>() -> u32 {
    const { assert!(size_of::<T>() <= u32::MAX as usize) };
    size_of::<T>() as u32
}

/// `size_of::<T>()` as the `u16` byte count of a structure whose size field is 16 bits, such as
/// `DEVMODEW`, checked at compile time to fit.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the size is asserted to fit 16 bits at compile time"
)]
pub(crate) const fn cb_size_u16<T>() -> u16 {
    const { assert!(size_of::<T>() <= u16::MAX as usize) };
    size_of::<T>() as u16
}

/// The text of a fixed-length UTF-16 field, up to its first NUL or its end.
pub(crate) fn utf16_until_nul(units: &[u16]) -> String {
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_byte_counts() {
        assert_eq!(cb_size::<u8>(), 1);
        assert_eq!(cb_size::<[f32; 4]>(), 16);
        assert_eq!(cb_size_u16::<[u8; 3]>(), 3);
    }

    #[test]
    fn text_stops_at_the_first_nul() {
        let mut units = [0u16; 32];
        for (unit, byte) in units.iter_mut().zip(br"\.\DISPLAY1") {
            *unit = u16::from(*byte);
        }
        assert_eq!(utf16_until_nul(&units), r"\.\DISPLAY1");
    }

    #[test]
    fn text_without_a_nul_runs_to_the_end() {
        let units: Vec<u16> = "NVIDIA".encode_utf16().collect();
        assert_eq!(utf16_until_nul(&units), "NVIDIA");
    }
}
