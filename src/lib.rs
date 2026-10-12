//! hindsight keeps the last moments of a game in a replay buffer, captured and encoded on the
//! GPU, and saves them as a clip when the hotkey is pressed.

#![cfg_attr(not(windows), expect(dead_code))]

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;

mod audio;
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

    let startup = startup();
    if let Err(error) = &startup
        && already_running(error)
        && sys::instance::show_running()
    {
        return Ok(());
    }
    gui::run(startup, tray)
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!(UNSUPPORTED_PLATFORM_MESSAGE)
}

#[cfg(windows)]
fn startup() -> Result<(service::Worker, config::Config)> {
    let store = storage::Store::open().context("open application state")?;
    mirror_log();
    let config = store.load_config().context("load configuration")?;
    config.validate().context("check configuration")?;
    let worker =
        service::Worker::spawn(store, config.clone()).context("start the recording worker")?;
    Ok((worker, config))
}

#[cfg(windows)]
fn mirror_log() {
    let log_file = storage::state_directory().join(log::FILE_NAME);
    if let Err(error) = log::mirror_to(&log_file) {
        log::warning(format!(
            "The log file {} could not be created: {error}",
            log_file.display()
        ));
    }
}

#[cfg(windows)]
fn already_running(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<storage::StorageError>(),
            Some(storage::StorageError::Locked)
        )
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn only_a_held_lock_counts_as_another_copy_running() {
        let locked: Result<()> =
            Err(storage::StorageError::Locked).context("open application state");
        assert!(already_running(&locked.unwrap_err()));

        let other: Result<()> =
            Err(storage::StorageError::Oversized).context("open application state");
        assert!(!already_running(&other.unwrap_err()));
        assert!(!already_running(&anyhow::anyhow!("load configuration")));
    }
}
