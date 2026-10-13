//! hindsight keeps the last moments of a game in a replay buffer, captured and encoded on the
//! GPU, and saves them as a clip when the hotkey is pressed.
//!
//! Off Windows `run` only refuses, so outside tests the portable modules are expected to be dead.
//! Their test build still checks dead code, excusing only the items that just Windows code uses.

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;

#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod audio;
#[cfg(windows)]
mod cli;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod config;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod gui;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod log;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod replay;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod service;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod storage;
#[cfg_attr(all(not(windows), not(test)), expect(dead_code))]
mod sys;

#[cfg(windows)]
pub(crate) const APP_NAME: &str = "hindsight";

#[cfg(not(windows))]
const UNSUPPORTED_PLATFORM_MESSAGE: &str =
    "hindsight requires Windows 10 1903 or newer and an NVIDIA GPU";

/// An error is also shown in a message box: the GUI subsystem has no console for `main`'s
/// report.
#[cfg(windows)]
pub fn run() -> Result<()> {
    log::install();
    log_panics();
    launch().inspect_err(|error| {
        let message = format!("{error:#}");
        log::error(&message);
        cli::show_error(&message);
    })
}

#[cfg(windows)]
fn launch() -> Result<()> {
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

/// The GUI subsystem has no console, so a panic on any thread is written to the journal (and
/// its file mirror) before the previous hook runs.
#[cfg(windows)]
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let payload = info.payload_as_str().unwrap_or("a panic without a message");
        let location = info
            .location()
            .map_or_else(|| "an unknown location".to_owned(), ToString::to_string);
        log::error(format!(
            "The thread {} panicked at {location}: {payload}",
            thread.name().unwrap_or("<unnamed>")
        ));
        previous(info);
    }));
}

#[cfg(windows)]
fn startup() -> Result<(service::Worker, config::Config)> {
    let store = storage::Store::open().context("open application state")?;
    mirror_log();
    let config = store.load_config().context("load configuration")?;
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
