//! Command-line entry points.

use crate::{
    APP_NAME,
    sys::{icon, startup::TRAY_FLAG_NAME},
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use winsafe::{self as w, co, prelude::*};

#[derive(Parser)]
#[command(name = APP_NAME, version, about)]
struct Arguments {
    /// Start hidden in the notification area.
    #[arg(long = TRAY_FLAG_NAME)]
    tray: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write the application icons as Windows .ico files, one per theme, from the logos under
    /// `<assets>/logo` into `<assets>`.
    Icon { assets: PathBuf },
}

pub(crate) enum Launch {
    Handled,
    Window { tray: bool },
}

/// Help, the version and usage errors go to a message box: the GUI subsystem has no console
/// for clap to print to.
pub(crate) fn dispatch() -> Result<Launch> {
    let arguments = match Arguments::try_parse() {
        Ok(arguments) => arguments,
        Err(error) if error.use_stderr() => bail!(error.render().to_string()),
        Err(answer) => {
            show(&answer.render().to_string(), co::MB::ICONINFORMATION);
            return Ok(Launch::Handled);
        }
    };
    match arguments.command {
        Some(Command::Icon { assets }) => {
            icon::write_all(&assets)
                .with_context(|| format!("write the icons under {}", assets.display()))?;
            Ok(Launch::Handled)
        }
        None => Ok(Launch::Window {
            tray: arguments.tray,
        }),
    }
}

/// For an error that ends the process before any window exists.
pub(crate) fn show_error(message: &str) {
    show(message, co::MB::ICONERROR);
}

fn show(message: &str, icon: co::MB) {
    // Nothing is left to report a failure to.
    let _ = w::HWND::NULL.MessageBox(message.trim_end(), APP_NAME, co::MB::OK | icon);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_version_are_answers_and_a_bad_flag_is_an_error() {
        let parse = |argument: &str| Arguments::try_parse_from([APP_NAME, argument]);
        for answer in ["--help", "--version"] {
            let error = parse(answer).err().unwrap();
            assert!(!error.use_stderr(), "{answer}");
            assert!(!error.render().to_string().is_empty(), "{answer}");
        }
        assert!(parse("--bogus").err().unwrap().use_stderr());
        assert!(parse("--tray").unwrap().tray);
    }
}
