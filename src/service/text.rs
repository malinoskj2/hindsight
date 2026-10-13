//! The worker's log lines, some of which the Status tab also shows.
//!
//! A whole line starts with a capital; a fragment that completes another line, such as the
//! reason after "Capture stopped:", does not. Lines with nothing to fill in are constants; the
//! others are formatted only when they are written.

use crate::{config::Config, replay::Stream};
use std::{
    fmt::{self, Display},
    path::Path,
};

// The settings.

pub(super) fn settings_applied(config: &Config) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Settings applied: {}, {}, {}, {} s buffer {}, {} fps, {} Mbps, hotkey {}",
            config.mode,
            config.backend,
            config.codec,
            config.buffer_seconds,
            config.buffer_memory.place(),
            config.frame_rate,
            config.bitrate_mbps,
            config.hotkey
        )
    })
}

pub(super) fn save_settings_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Could not save the settings: {error}"))
}

// The session.

pub(super) const RESTARTING: &str = "Starting capture again";
pub(super) const PAUSED: &str = "Capture paused; what is buffered stays saveable";
pub(super) const RESUMED: &str = "Capture resumed";
pub(super) const RESIZED: &str =
    "The display or window changed size; capture restarted and the replay buffer starts over";
pub(super) const DISPLAY_GONE: &str = "the display went away";
pub(super) const NO_FRAMES: &str = "no frames arrived from the display";
pub(super) const NO_PANIC_MESSAGE: &str = "it gave no message";
pub(super) const WORKER_GONE: &str = "The recording worker has stopped, so the request was dropped";

pub(super) fn capture_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "the capture thread panicked: {message}"))
}

pub(super) fn worker_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "The recording worker panicked and stopped; start hindsight again to record: {message}"
        )
    })
}

pub(super) fn target_found(name: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "{name} is full screen; recording it"))
}

pub(super) fn target_closed(name: &str) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "{name} closed; the replay buffer stays until the next full-screen app"
        )
    })
}

pub(super) fn falling_back_to_monitor(name: &str) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "{name} cannot be captured as a window; capturing its monitor instead"
        )
    })
}

pub(super) fn recording(stream: &Stream, config: &Config) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Recording {} {}×{} at {} fps, {} Mbps, keeping {} s",
            stream.codec,
            stream.width,
            stream.height,
            stream.frame_rate,
            config.bitrate_mbps,
            config.buffer_seconds
        )
    })
}

pub(super) fn session_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Capture stopped: {error}"))
}

pub(super) fn capture_thread_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "the capture thread could not start: {error}"))
}

// Saving.

pub(crate) const NOTHING_TO_SAVE: &str =
    "Nothing to save yet; the replay buffer fills once capture starts.";
pub(super) const ALREADY_SAVING: &str = "A clip is already being saved";

pub(super) fn save_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "the save thread panicked: {message}"))
}

pub(super) fn clip_saved(seconds: f64, path: &Path) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Saved a {seconds:.1} s clip to {}", path.display()))
}

pub(super) fn save_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Could not save the clip: {error}"))
}

#[cfg(windows)]
pub(super) fn background_save_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "The clip is being written at normal priority; background mode was refused: {error}"
        )
    })
}

// The totals.

pub(super) fn clip_size_unreadable(path: &Path, error: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "The size of {} could not be read, so the clip counts as nothing: {error}",
            path.display()
        )
    })
}

pub(super) fn load_totals_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "The lifetime totals could not be read, so they are not kept: {error}"
        )
    })
}

pub(super) fn save_totals_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "The lifetime totals could not be saved: {error}"))
}
