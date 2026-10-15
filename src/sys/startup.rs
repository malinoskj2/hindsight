//! The per-user "Start with Windows" entry in the registry `Run` key.

use crate::APP_NAME;
use std::{
    ffi::{OsStr, OsString},
    io,
};
use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
};

/// The name of the flag that starts the app hidden in the tray. The command-line parser declares
/// the flag from this constant.
pub(crate) const TRAY_FLAG_NAME: &str = "tray";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub(crate) fn enabled() -> io::Result<bool> {
    let value = run_key(KEY_READ).and_then(|key| key.get_value::<String, _>(APP_NAME));
    match value {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn set(enabled: bool) -> io::Result<()> {
    if enabled {
        return add();
    }
    let removed = run_key(KEY_READ | KEY_WRITE).and_then(|key| key.delete_value(APP_NAME));
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn add() -> io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)?;
    let executable = std::env::current_exe()?;
    key.set_value(APP_NAME, &command(executable.as_os_str()))
}

/// Opens the `Run` key with `access`, a `KEY_*` mask. `winreg` names the mask's type only
/// through `windows-sys`.
fn run_key(access: u32) -> io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, access)
}

fn command(executable: &OsStr) -> OsString {
    let mut command = OsString::from("\"");
    command.push(executable);
    command.push("\" --");
    command.push(TRAY_FLAG_NAME);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_startup_command_quotes_the_path_and_starts_in_the_tray() {
        assert_eq!(
            command(OsStr::new(r"C:\Program Files\hindsight\hindsight.exe")),
            r#""C:\Program Files\hindsight\hindsight.exe" --tray"#
        );
    }
}
