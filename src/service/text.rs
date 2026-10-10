use crate::{config::Config, replay::Stream};
use std::{fmt::Display, path::Path};

pub(super) const NOTHING_TO_SAVE: &str =
    "Nothing to save yet; the replay buffer fills once capture starts.";
pub(super) const RESIZED: &str =
    "The display changed size; capture restarted and the replay buffer starts over.";

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

pub(super) fn save_settings_failed(error: &impl Display) -> String {
    format!("Could not save the settings: {error}")
}

pub(super) fn session_failed(error: &str) -> String {
    format!("Capture stopped: {error}")
}

pub(super) fn saved(seconds: f64, path: &Path) -> String {
    format!("Saved a {seconds:.1} s clip to {}", path.display())
}

pub(super) fn save_failed(error: &impl Display) -> String {
    format!("Could not save the clip: {error}")
}
