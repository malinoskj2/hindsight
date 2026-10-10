//! Everything the window shows in words, its own log lines included. Kept apart from the pages
//! so the layout reads without the wording, and the wording can change without the layout.

use crate::service::{DisplayStatus, Status};
use std::{fmt::Display, path::Path};

/// Shown in place of a value that is not known yet.
const NO_VALUE: &str = "—";

pub(super) const TAB_STATUS: &str = "Status";
pub(super) const TAB_SETTINGS: &str = "Settings";
pub(super) const TAB_LOGS: &str = "Logs";

pub(super) const GROUP_RECORDING: &str = "Recording";
pub(super) const GROUP_CLIPS: &str = "Clips";
pub(super) const GROUP_RECORDING_SETTINGS: &str = "Recording";
pub(super) const GROUP_STARTUP: &str = "Startup";

pub(super) const ENCODER: &str = "Encoder";
pub(super) const CAPTURE_RATE: &str = "Frame rate";
pub(super) const BUFFERED: &str = "Buffered";
pub(super) const LAST_CLIP: &str = "Last clip";
pub(super) const CLIP_FOLDER: &str = "Folder";
pub(super) const SAVE_REPLAY: &str = "Save replay";
pub(super) const SAVING: &str = "Saving…";
pub(super) const STOP_RECORDING: &str = "Stop recording";
pub(super) const START_RECORDING: &str = "Start recording";

pub(super) const CODEC: &str = "Codec";
pub(super) const FRAME_RATE: &str = "Frame rate";
pub(super) const BUFFER_SECONDS: &str = "Buffer (s)";
pub(super) const BITRATE: &str = "Bitrate (Mbps)";
pub(super) const HOTKEY: &str = "Save hotkey";
pub(super) const FOLDER: &str = "Clip folder";
pub(super) const APPLY_AND_RESTART: &str = "Apply and restart";
pub(super) const RESTART_NOTICE: &str =
    "Applying restarts recording; save any wanted replay first.";
pub(super) const NUMBER_REQUIRED: &str =
    "Enter whole numbers for the frame rate, buffer and bitrate.";
pub(super) const START_WITH_WINDOWS: &str = "Start with Windows";
pub(super) const START_WITH_WINDOWS_HELP: &str =
    "Starts hidden in the notification area at sign-in and begins recording right away.";

pub(super) const STARTUP_ENABLED: &str = "Start with Windows enabled";
pub(super) const STARTUP_DISABLED: &str = "Start with Windows disabled";
pub(super) const QUIT_FROM_TRAY: &str = "Quit chosen from the tray menu";
pub(super) const CLOSED_TO_TRAY: &str = "Window closed; recording continues from the tray";

pub(super) const STARTUP_FAILED_TITLE: &str = "dxgi could not start";
pub(super) const STARTUP_FAILED_HELP: &str =
    "Close this window, resolve the error, then launch again.";

pub(super) fn tray_unavailable(error: impl Display) -> String {
    format!("Tray unavailable; closing this window stops recording and quits: {error}")
}

pub(super) fn startup_setting_failed(error: impl Display) -> String {
    format!("Could not update the Start with Windows entry: {error}")
}

pub(super) fn status(state: DisplayStatus) -> String {
    let message = match state {
        DisplayStatus::Starting => "Starting capture…",
        DisplayStatus::Recording => "Recording to the replay buffer",
        DisplayStatus::Retrying => "Capture stopped · retrying shortly",
        DisplayStatus::Stopped => "Stopped · the replay buffer is off",
        DisplayStatus::Unsupported => "Cannot record with these settings",
    };
    format!("●  {message}")
}

pub(super) fn encoder(status: &Status) -> String {
    status.stream.map_or_else(
        || NO_VALUE.to_owned(),
        |stream| {
            format!(
                "{} · {}×{} · {} Mbps",
                stream.codec, stream.width, stream.height, status.bitrate_mbps
            )
        },
    )
}

pub(super) fn rates(status: &Status) -> String {
    match (status.encode_rate, status.fresh_rate) {
        (Some(encoded), Some(fresh)) => {
            format!("{encoded:.1} fps encoded · {fresh:.1} fps of new frames")
        }
        _ => NO_VALUE.to_owned(),
    }
}

pub(super) fn buffered(status: &Status) -> String {
    format!(
        "{:.1} s · {:.1} MB",
        status.buffered_seconds,
        status.buffered_bytes as f64 / 1e6
    )
}

pub(super) fn last_clip(path: Option<&Path>) -> String {
    path.map_or_else(|| NO_VALUE.to_owned(), |path| path.display().to_string())
}

pub(super) fn hotkey_hint(hotkey: &str) -> String {
    format!("or press {hotkey}")
}

pub(super) fn buffer_estimate(megabytes: u64) -> String {
    format!("Buffer memory at this bitrate: about {megabytes} MB")
}
