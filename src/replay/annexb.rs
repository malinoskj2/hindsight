//! Annex B streams, the form NVENC writes H.264 and HEVC in: NAL units separated by start codes.

/// NAL units of an Annex B stream, without their start codes.
pub(super) fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index..index + 3] == [0, 0, 1] {
            if let Some(start) = start {
                units.push(trim_trailing_zeros(&stream[start..index]));
            }
            index += 3;
            start = Some(index);
        } else {
            index += 1;
        }
    }
    if let Some(start) = start {
        units.push(trim_trailing_zeros(&stream[start..]));
    }
    units.retain(|unit| !unit.is_empty());
    units
}

/// The unit's payload with emulation prevention bytes (the `03` in `00 00 03`) removed, up to
/// `limit` bytes; enough to read the fixed-position fields at the start of a parameter set.
pub(super) fn unescape(unit: &[u8], limit: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(limit.min(unit.len()));
    let mut zeros = 0;
    for &byte in unit {
        if payload.len() == limit {
            break;
        }
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        payload.push(byte);
    }
    payload
}

/// Splits a stream into access units, each starting at a NAL unit `starts_unit` accepts.
#[cfg(test)]
pub(super) fn access_units(stream: &[u8], starts_unit: impl Fn(&[u8]) -> bool) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = (0..stream.len().saturating_sub(3))
        .filter(|&index| stream[index..index + 3] == [0, 0, 1] && starts_unit(&stream[index + 3..]))
        // A four-byte start code begins one byte earlier.
        .map(|index| {
            if index > 0 && stream[index - 1] == 0 {
                index - 1
            } else {
                index
            }
        })
        .collect();
    starts.push(stream.len());
    starts
        .windows(2)
        .map(|pair| &stream[pair[0]..pair[1]])
        .collect()
}

/// The zero byte of a four-byte start code belongs to the start code, not the unit before it.
fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    &unit[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 9, 16, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65, 7];
        assert_eq!(
            nal_units(&stream),
            [&[9, 16][..], &[0x67, 1, 2], &[0x65, 7]]
        );
        assert_eq!(nal_units(&[0, 0, 0, 0]).len(), 0);
        assert_eq!(nal_units(&[]).len(), 0);
    }

    #[test]
    fn removes_emulation_prevention_bytes() {
        assert_eq!(
            unescape(&[1, 0, 0, 3, 1, 0, 0, 3, 0, 5], 16),
            [1, 0, 0, 1, 0, 0, 0, 5]
        );
        assert_eq!(unescape(&[0, 3, 0, 0, 3], 16), [0, 3, 0, 0]);
        assert_eq!(unescape(&[1, 2, 3, 4], 2), [1, 2]);
    }
}
