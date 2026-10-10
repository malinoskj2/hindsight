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

/// The command-line flag, without its dashes, that starts hidden in the tray, since nobody is
/// there to look at the window at logon. The command-line parser declares it from this too.
pub(crate) const TRAY_FLAG_NAME: &str = "tray";
/// The key whose values Windows runs at logon, under `HKEY_CURRENT_USER`.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Whether the entry is there.
pub(crate) fn enabled() -> io::Result<bool> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)?;
    match key.get_value::<String, _>(APP_NAME) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Adds the entry for this executable, or removes it.
pub(crate) fn set(enabled: bool) -> io::Result<()> {
    let key =
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)?;
    if !enabled {
        // An entry that is already gone is what was asked for.
        return match key.delete_value(APP_NAME) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    let executable = std::env::current_exe()?;
    key.set_value(APP_NAME, &command(executable.as_os_str()))
}

/// The entry's command line: the quoted executable, started in the tray.
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
