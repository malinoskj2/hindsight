//! The NvFBC key: the private data NVIDIA issues to a licensed vendor, which the driver
//! requires before it lets an application capture with NvFBC on a GeForce GPU. It is entered
//! as hexadecimal when the NvFBC backend is chosen and kept as binary in the user's registry,
//! outside the settings file, so a shared or synced configuration never carries it.

use crate::APP_NAME;
use std::{borrow::Cow, fmt::Write, io};
use thiserror::Error;
use winreg::{
    RegKey, RegValue,
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType},
};

/// The longest key accepted, in bytes; NVIDIA's are far shorter.
pub(crate) const MAX_BYTES: usize = 256;
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
}

/// The key's bytes from its hexadecimal spelling; spaces and line breaks are ignored.
pub(crate) fn parse(text: &str) -> Result<Vec<u8>, ParseError> {
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
        assert_eq!(parse("0x12"), Err(ParseError::NotHex));
        assert_eq!(parse("12-34"), Err(ParseError::NotHex));
        assert_eq!(
            parse(&"00".repeat(MAX_BYTES)).map(|key| key.len()),
            Ok(MAX_BYTES)
        );
        assert_eq!(parse(&"00".repeat(MAX_BYTES + 1)), Err(ParseError::TooLong));
    }

    #[test]
    fn formatting_round_trips() {
        let key = [0x00, 0x7F, 0x80, 0xFF];
        assert_eq!(format(&key), "007f80ff");
        assert_eq!(parse(&format(&key)), Ok(key.to_vec()));
    }
}
