//! Annex B streams, the form NVENC writes H.264 and HEVC in: NAL units separated by start codes.

/// The three-byte start code; a four-byte one has a zero byte in front.
const START_CODE: [u8; 3] = [0, 0, 1];
/// The byte inserted after two zero bytes so that a payload never contains a start code.
const EMULATION_PREVENTION: u8 = 3;

/// NAL units of an Annex B stream, without their start codes; empty ones are left out. Lazy,
/// since telling a keyframe needs only the first few units of every frame.
pub(super) fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    // The stream from the next start code on, or nothing once the last unit has been given.
    let mut rest = start_code(stream).map(|at| &stream[at..]);
    std::iter::from_fn(move || {
        loop {
            let after_code = &rest?[START_CODE.len()..];
            let next = start_code(after_code);
            rest = next.map(|at| &after_code[at..]);
            let unit = trim_trailing_zeros(&after_code[..next.unwrap_or(after_code.len())]);
            if !unit.is_empty() {
                return Some(unit);
            }
        }
    })
}

/// Where the first start code in `data` begins.
fn start_code(data: &[u8]) -> Option<usize> {
    data.windows(START_CODE.len())
        .position(|window| window == START_CODE)
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
        if zeros >= 2 && byte == EMULATION_PREVENTION {
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
    let mut starts: Vec<usize> = (0..stream.len().saturating_sub(START_CODE.len()))
        .filter(|&index| {
            stream[index..].starts_with(&START_CODE)
                && starts_unit(&stream[index + START_CODE.len()..])
        })
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
            nal_units(&stream).collect::<Vec<_>>(),
            [&[9, 16][..], &[0x67, 1, 2], &[0x65, 7]]
        );
        assert_eq!(nal_units(&[0, 0, 0, 0]).count(), 0);
        assert_eq!(nal_units(&[]).count(), 0);
        // Start codes with nothing between them, and a trailing one, yield no units.
        assert_eq!(
            nal_units(&[0, 0, 1, 0, 0, 1, 5, 0, 0, 1]).collect::<Vec<_>>(),
            [&[5][..]]
        );
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
