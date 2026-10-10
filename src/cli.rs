//! Command-line entry points: `--tray` starts hidden in the notification area (the Start with
//! Windows entry uses it), and `icon <path>` writes the application icon for the build.

use crate::{
    APP_NAME,
    sys::{icon, startup::TRAY_FLAG_NAME},
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    /// Write the application icon as a Windows .ico file.
    Icon { path: PathBuf },
}

/// What the command line asked for.
pub(crate) enum Launch {
    /// A command ran; there is no window to show.
    Handled,
    /// The application starts, hidden in the notification area when `tray` is set.
    Window { tray: bool },
}

/// Parses the command line and runs a command it names.
pub(crate) fn dispatch() -> Result<Launch> {
    let arguments = Arguments::parse();
    match arguments.command {
        Some(Command::Icon { path }) => {
            std::fs::write(&path, icon::ico())
                .with_context(|| format!("write {}", path.display()))?;
            Ok(Launch::Handled)
        }
        None => Ok(Launch::Window {
            tray: arguments.tray,
        }),
    }
}
