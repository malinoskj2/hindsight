#![cfg_attr(not(windows), expect(dead_code))]

#[cfg(windows)]
mod cli;
mod config;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod gui;
mod log;
mod replay;
mod service;
mod storage;
mod sys;

pub(crate) const APP_NAME: &str = "hindsight";

#[cfg(not(windows))]
const UNSUPPORTED_PLATFORM_MESSAGE: &str =
    "hindsight requires Windows 10 1903 or newer and an NVIDIA GPU";

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;

/// Start the desktop application and its recording worker.
#[cfg(windows)]
pub fn run() -> Result<()> {
    let tray = match cli::dispatch()? {
        cli::Launch::Handled => return Ok(()),
        cli::Launch::Window { tray } => tray,
    };

    log::info(format!(
        "{APP_NAME} {} starting on {}",
        env!("CARGO_PKG_VERSION"),
        log::today()
    ));

    gui::run(startup(), tray)
}

/// Opens the state and log file, loads the configuration and starts the recording worker.
#[cfg(windows)]
fn startup() -> Result<(service::Worker, config::Config)> {
    let store = storage::Store::open().context("open application state")?;
    let log_file = storage::state_directory().join(log::FILE_NAME);
    if let Err(error) = log::mirror_to(&log_file) {
        log::warning(format!(
            "The log file {} could not be created: {error}",
            log_file.display()
        ));
    }
    let config = store.load_config().context("load configuration")?;
    config.validate().context("check configuration")?;
    let worker = service::Worker::spawn(store, config.clone());
    Ok((worker, config))
}

/// The recording application is available on Windows only.
#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!(UNSUPPORTED_PLATFORM_MESSAGE)
}
