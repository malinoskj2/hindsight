//! Command-line entry points.

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
    /// Write the application icons as Windows .ico files, one per theme, from the logos under
    /// `<assets>/logo` into `<assets>`.
    Icon { assets: PathBuf },
}

pub(crate) enum Launch {
    Handled,
    Window { tray: bool },
}

pub(crate) fn dispatch() -> Result<Launch> {
    let arguments = Arguments::parse();
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
