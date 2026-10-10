//! The worker's log lines, some of which the Status tab also shows. Kept apart from the worker
//! so its logic reads without the wording, and the wording can change without touching it.

use crate::{config::Config, replay::Stream};
use std::{fmt::Display, path::Path};

pub(super) const NOTHING_TO_SAVE: &str =
    "Nothing to save yet; the replay buffer fills once capture starts.";
pub(super) const RESIZED: &str =
    "The display changed size; capture restarted and the replay buffer starts over.";
pub(super) const RECORDING_STOPPED: &str = "Recording stopped";
pub(super) const RESTARTING: &str = "Starting capture again";
pub(super) const ALREADY_SAVING: &str = "A clip is already being saved";
pub(super) const CAPTURE_PANICKED: &str = "the capture thread panicked";
pub(super) const SAVE_PANICKED: &str = "the save thread panicked";

pub(super) fn recording(stream: Option<&Stream>, config: &Config) -> String {
    stream.map_or_else(
        || "Recording".to_owned(),
        |stream| {
            format!(
                "Recording {} {}×{} at {} fps, {} Mbps, keeping {} s",
                stream.codec,
                stream.width,
                stream.height,
                stream.frame_rate,
                config.bitrate_mbps,
                config.buffer_seconds
            )
        },
    )
}

pub(super) fn settings_applied(config: &Config) -> String {
    format!(
        "Settings applied: {}, {} s buffer, {} fps, {} Mbps, hotkey {}",
        config.codec, config.buffer_seconds, config.frame_rate, config.bitrate_mbps, config.hotkey
    )
}

pub(super) fn save_settings_failed(error: impl Display) -> String {
    format!("Could not save the settings: {error}")
}

pub(super) fn session_failed(error: &str) -> String {
    format!("Capture stopped: {error}")
}

pub(super) fn capture_thread_failed(error: impl Display) -> String {
    format!("start the capture thread: {error}")
}

pub(super) fn saved(seconds: f64, path: &Path) -> String {
    format!("Saved a {seconds:.1} s clip to {}", path.display())
}

pub(super) fn background_save_failed(error: impl Display) -> String {
    format!("The clip is being written at normal priority; background mode was refused: {error}")
}

pub(super) fn save_failed(error: impl Display) -> String {
    format!("Could not save the clip: {error}")
}
