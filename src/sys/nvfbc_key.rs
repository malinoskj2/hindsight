//! The NvFBC key: the private data NVIDIA issues to a licensed vendor, which the driver
//! requires before it lets an application capture with NvFBC on a GeForce GPU. It is entered
//! as hexadecimal or as four 0x-prefixed dwords when the NvFBC backend is chosen, and kept as
//! binary in the user's registry, outside the settings file, so a shared or synced
//! configuration never carries it.

use crate::APP_NAME;
use std::{borrow::Cow, fmt::Write, io};
use thiserror::Error;
use winreg::{
    RegKey, RegValue,
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType},
};

/// The longest key accepted, in bytes; NVIDIA's are far shorter.
pub(crate) const MAX_BYTES: usize = 256;
/// The number of `0x`-prefixed dwords accepted as a key.
const DWORD_COUNT: usize = 4;
/// The registry value the key is kept in, under `app_key`.
const VALUE_NAME: &str = "NvFBCKey";

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ParseError {
    #[error("enter the NvFBC key NVIDIA issued you")]
    Empty,
    #[error("the NvFBC key must be hexadecimal digits (0-9, a-f)")]
    NotHex,
    #[error("the NvFBC key must be whole bytes: an even number of hexadecimal digits")]
    OddLength,
    #[error("the NvFBC key can be at most {MAX_BYTES} bytes")]
    TooLong,
    #[error(
        "the NvFBC key must be {DWORD_COUNT} dwords, each 0x followed by 1 to 8 hexadecimal digits"
    )]
    DwordCount,
    #[error("each NvFBC key dword must be 0x followed by 1 to 8 hexadecimal digits")]
    NotDword,
}

/// The key's bytes from its spelling: either hexadecimal digits, with spaces and line breaks
/// ignored, or four `0x`-prefixed dwords separated by spaces or commas. Each dword is stored
/// most significant byte first, as written.
pub(crate) fn parse(text: &str) -> Result<Vec<u8>, ParseError> {
    let tokens = text
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }
    if tokens.iter().any(|token| strip_hex_prefix(token).is_some()) {
        return parse_dwords(&tokens);
    }
    parse_hex(text)
}

/// The digits after a `0x` or `0X` prefix, if the token has one.
fn strip_hex_prefix(token: &str) -> Option<&str> {
    token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
}

/// The key's bytes from `DWORD_COUNT` dwords, each written `0x` followed by 1 to 8 digits.
fn parse_dwords(tokens: &[&str]) -> Result<Vec<u8>, ParseError> {
    if tokens.len() != DWORD_COUNT {
        return Err(ParseError::DwordCount);
    }
    let mut key = Vec::with_capacity(DWORD_COUNT * size_of::<u32>());
    for token in tokens {
        let digits = strip_hex_prefix(token)
            .filter(|digits| (1..=8).contains(&digits.len()))
            .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(ParseError::NotDword)?;
        let value = u32::from_str_radix(digits, 16).map_err(|_| ParseError::NotDword)?;
        key.extend_from_slice(&value.to_be_bytes());
    }
    Ok(key)
}

/// The key's bytes from hexadecimal digits, two per byte, with whitespace ignored.
fn parse_hex(text: &str) -> Result<Vec<u8>, ParseError> {
    let digits = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| {
            character
                .to_digit(16)
                .and_then(|digit| u8::try_from(digit).ok())
                .ok_or(ParseError::NotHex)
        })
        .collect::<Result<Vec<u8>, _>>()?;
    if digits.is_empty() {
        return Err(ParseError::Empty);
    }
    if digits.len() % 2 != 0 {
        return Err(ParseError::OddLength);
    }
    if digits.len() / 2 > MAX_BYTES {
        return Err(ParseError::TooLong);
    }
    let (pairs, _) = digits.as_chunks::<2>();
    Ok(pairs.iter().map(|[high, low]| (high << 4) | low).collect())
}

/// The key in the spelling `parse` reads.
pub(crate) fn format(key: &[u8]) -> String {
    key.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The saved key, if one has been entered.
pub(crate) fn load() -> io::Result<Option<Vec<u8>>> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(app_key(), KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match key.get_raw_value(VALUE_NAME) {
        Ok(value) if value.vtype == RegType::REG_BINARY && !value.bytes.is_empty() => {
            Ok(Some(value.bytes.into_owned()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Saves the key, replacing the one entered before.
pub(crate) fn save(key: &[u8]) -> io::Result<()> {
    let (app, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(app_key(), KEY_READ | KEY_WRITE)?;
    app.set_raw_value(
        VALUE_NAME,
        &RegValue {
            bytes: Cow::Borrowed(key),
            vtype: RegType::REG_BINARY,
        },
    )
}

/// `HKEY_CURRENT_USER\Software\hindsight`.
fn app_key() -> String {
    format!(r"Software\{APP_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_read_as_hexadecimal_bytes() {
        assert_eq!(parse("00ff1A"), Ok(vec![0x00, 0xFF, 0x1A]));
        assert_eq!(parse(" 0a 0b\r\n0c "), Ok(vec![0x0A, 0x0B, 0x0C]));
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("  \n"), Err(ParseError::Empty));
        assert_eq!(parse("abc"), Err(ParseError::OddLength));
        assert_eq!(parse("0x12"), Err(ParseError::DwordCount));
        assert_eq!(parse("12-34"), Err(ParseError::NotHex));
        assert_eq!(
            parse(&"00".repeat(MAX_BYTES)).map(|key| key.len()),
            Ok(MAX_BYTES)
        );
        assert_eq!(parse(&"00".repeat(MAX_BYTES + 1)), Err(ParseError::TooLong));
    }

    #[test]
    fn keys_are_read_as_four_prefixed_dwords() {
        assert_eq!(
            parse("0x01020304 0x0A0b0C0d 0xFFFFFFFF 0x0"),
            Ok(vec![
                1, 2, 3, 4, 0x0A, 0x0B, 0x0C, 0x0D, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0
            ])
        );
        assert_eq!(
            parse("0x1,0X2, 0x3\n0x4"),
            Ok(vec![0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4])
        );
        assert_eq!(parse("0x1 0x2 0x3"), Err(ParseError::DwordCount));
        assert_eq!(parse("0x1 0x2 0x3 0x4 0x5"), Err(ParseError::DwordCount));
        assert_eq!(parse("0x1 0x2 0x3 0x123456789"), Err(ParseError::NotDword));
        assert_eq!(parse("0x 0x2 0x3 0x4"), Err(ParseError::NotDword));
        assert_eq!(parse("0x1 0x2 0x3 +4"), Err(ParseError::NotDword));
        assert_eq!(parse("0x1 0x2 0x3 0xG4"), Err(ParseError::NotDword));
        assert_eq!(parse("0x1 2 3 4"), Err(ParseError::NotDword));
    }

    #[test]
    fn formatted_keys_read_back_the_same() {
        let key = [0x00, 0x7F, 0x80, 0xFF];
        assert_eq!(format(&key), "007f80ff");
        assert_eq!(parse(&format(&key)), Ok(key.to_vec()));
    }
}
