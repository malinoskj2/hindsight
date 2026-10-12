//! The worker's log lines, some of which the Status tab also shows. Kept apart from the worker
//! so its logic reads without the wording, and the wording can change without touching it.
//! They are grouped as the worker meets them: the settings, the session, saving and the totals.

use crate::{config::Config, replay::Stream};
use std::{fmt::Display, path::Path};

// The settings.

pub(super) fn settings_applied(config: &Config) -> String {
    format!(
        "Settings applied: {}, {}, {}, {} s buffer in {}, {} fps, {} Mbps, hotkey {}",
        config.mode,
        config.backend,
        config.codec,
        config.buffer_seconds,
        config.buffer_memory.short_name(),
        config.frame_rate,
        config.bitrate_mbps,
        config.hotkey
    )
}

pub(super) fn save_settings_failed(error: impl Display) -> String {
    format!("Could not save the settings: {error}")
}

// The session.

pub(super) const RESTARTING: &str = "Starting capture again";
pub(super) const RESIZED: &str =
    "The display or window changed size; capture restarted and the replay buffer starts over.";
pub(super) const DISPLAY_GONE: &str = "the display went away";
pub(super) const NO_FRAMES: &str = "no frames arrived from the display";
pub(super) const CAPTURE_PANICKED: &str = "the capture thread panicked";

pub(super) fn target_found(name: &str) -> String {
    format!("{name} is full screen; recording it")
}

pub(super) fn target_closed(name: &str) -> String {
    format!("{name} closed; the replay buffer stays until the next full-screen app")
}

pub(super) fn falling_back_to_monitor(name: &str) -> String {
    format!("{name} cannot be captured as a window; capturing its monitor instead")
}

pub(super) fn recording(stream: &Stream, config: &Config) -> String {
    format!(
        "Recording {} {}×{} at {} fps, {} Mbps, keeping {} s",
        stream.codec,
        stream.width,
        stream.height,
        stream.frame_rate,
        config.bitrate_mbps,
        config.buffer_seconds
    )
}

pub(super) fn session_failed(error: impl Display) -> String {
    format!("Capture stopped: {error}")
}

pub(super) fn capture_thread_failed(error: impl Display) -> String {
    format!("start the capture thread: {error}")
}

// Saving.

pub(super) const NOTHING_TO_SAVE: &str =
    "Nothing to save yet; the replay buffer fills once capture starts.";
pub(super) const ALREADY_SAVING: &str = "A clip is already being saved";
pub(super) const SAVE_PANICKED: &str = "the save thread panicked";

pub(super) fn clip_saved(seconds: f64, path: &Path) -> String {
    format!("Saved a {seconds:.1} s clip to {}", path.display())
}

pub(super) fn save_failed(error: impl Display) -> String {
    format!("Could not save the clip: {error}")
}

pub(super) fn background_save_failed(error: impl Display) -> String {
    format!("The clip is being written at normal priority; background mode was refused: {error}")
}

// The totals.

pub(super) fn clip_size_unreadable(path: &Path, error: impl Display) -> String {
    format!(
        "the size of {} could not be read, so the clip counts as nothing: {error}",
        path.display()
    )
}

pub(super) fn load_totals_failed(error: impl Display) -> String {
    format!("the lifetime totals could not be read, so they are not kept: {error}")
}

pub(super) fn save_totals_failed(error: impl Display) -> String {
    format!("the lifetime totals could not be saved: {error}")
}
