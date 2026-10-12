//! Annex B streams, the form NVENC writes H.264 and HEVC in: NAL units separated by start codes.

/// The three-byte start code; a four-byte one has a zero byte in front.
const START_CODE: [u8; 3] = [0, 0, 1];
const EMULATION_PREVENTION: u8 = 3;

pub(super) fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
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

fn start_code(data: &[u8]) -> Option<usize> {
    data.windows(START_CODE.len())
        .position(|window| window == START_CODE)
}

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

#[cfg(test)]
pub(super) fn access_units(stream: &[u8], starts_unit: impl Fn(&[u8]) -> bool) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = (0..stream.len().saturating_sub(START_CODE.len()))
        .filter(|&index| {
            stream[index..].starts_with(&START_CODE)
                && starts_unit(&stream[index + START_CODE.len()..])
        })
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
