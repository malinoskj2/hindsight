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

pub(super) const SOURCE: &str = "Source";
pub(super) const GROUP_STATS: &str = "Stats";
pub(super) const LIFETIME_WRITTEN: &str = "Lifetime Written";
pub(super) const LIFETIME_SAVED: &str = "Lifetime Saved";
pub(super) const ENCODER: &str = "Encoder";
pub(super) const CAPTURE_RATE: &str = "Frame rate";
pub(super) const SESSION_WRITTEN: &str = "Session Written";
pub(super) const SESSION_SAVED: &str = "Session Saved";
pub(super) const LAST_CLIP: &str = "Last clip";
pub(super) const CLIP_FOLDER: &str = "Folder";
pub(super) const SAVE_REPLAY: &str = "Save replay";
pub(super) const SAVING: &str = "Saving…";
pub(super) const STOP_RECORDING: &str = "Stop recording";
pub(super) const START_RECORDING: &str = "Start recording";

pub(super) const MODE: &str = "Record";
pub(super) const BACKEND: &str = "Backend";
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
    "Starts hidden in the notification area at sign-in, ready for the first full-screen app.";
pub(super) const FOREGROUND_WATCH_FAILED: &str = "Full-screen apps are noticed every second rather than at once: the foreground hook could not be installed";

pub(super) const STARTUP_ENABLED: &str = "Start with Windows enabled";
pub(super) const STARTUP_DISABLED: &str = "Start with Windows disabled";
pub(super) const QUIT_FROM_TRAY: &str = "Quit chosen from the tray menu";
pub(super) const CLOSED_TO_TRAY: &str = "Window closed; recording continues from the tray";

pub(super) const NVFBC_KEY_TITLE: &str = "NvFBC key";
pub(super) const NVFBC_KEY_HELP: &str = "NvFBC capture is for vendors licensed by NVIDIA. Enter the key NVIDIA issued you, as hexadecimal or as four 0x-prefixed dwords; it is saved for this Windows account only.";
pub(super) const NVFBC_KEY: &str = "Key";
pub(super) const NVFBC_KEY_SAVE: &str = "Save";
pub(super) const NVFBC_KEY_CANCEL: &str = "Cancel";
pub(super) const NVFBC_KEY_SAVED: &str = "NvFBC key saved";

pub(super) const STARTUP_FAILED_TITLE: &str = "hindsight could not start";
pub(super) const STARTUP_FAILED_HELP: &str =
    "Close this window, resolve the error, then launch again.";

pub(super) fn tray_unavailable(error: impl Display) -> String {
    format!("Tray unavailable; closing this window stops recording and quits: {error}")
}

pub(super) fn startup_setting_failed(error: impl Display) -> String {
    format!("Could not update the Start with Windows entry: {error}")
}

pub(super) fn nvfbc_key_unreadable(error: impl Display) -> String {
    format!("The saved NvFBC key could not be read: {error}")
}

pub(super) fn nvfbc_key_not_saved(error: impl Display) -> String {
    format!("Could not save the NvFBC key: {error}")
}

pub(super) fn nvfbc_key_window_failed(error: impl Display) -> String {
    format!("The NvFBC key window could not be shown: {error}")
}

pub(super) fn status(state: DisplayStatus) -> String {
    let message = match state {
        DisplayStatus::Starting => "Starting capture…",
        DisplayStatus::Waiting => "Waiting for a full-screen app",
        DisplayStatus::Recording => "Recording to the replay buffer",
        DisplayStatus::Retrying => "Capture stopped · retrying shortly",
        DisplayStatus::Stopped => "Stopped · the replay buffer is off",
        DisplayStatus::Unsupported => "Cannot record with these settings",
    };
    format!("●  {message}")
}

pub(super) fn source(status: &Status) -> String {
    status.source.clone().unwrap_or_else(|| NO_VALUE.to_owned())
}

/// Bytes in a gigabyte here: 1024 cubed.
const GIGABYTE: f64 = 1_073_741_824.0;

/// A lifetime total in gigabytes, or a dash while it is not known.
#[expect(
    clippy::cast_precision_loss,
    reason = "shown to two decimals, far coarser than the rounding of a byte count to f64"
)]
fn gigabytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || NO_VALUE.to_owned(),
        |bytes| format!("{:.2} GB", bytes as f64 / GIGABYTE),
    )
}

/// Everything that has gone into the replay buffer since the app started.
pub(super) fn session_written(status: &Status) -> String {
    gigabytes(Some(status.session_written_bytes))
}

/// Everything saved as clips since the app started.
pub(super) fn session_saved(status: &Status) -> String {
    gigabytes(Some(status.session_saved_bytes))
}

/// Everything that has gone into the replay buffer over every run of the app.
pub(super) fn lifetime_written(status: &Status) -> String {
    gigabytes(status.buffered_lifetime_bytes)
}

/// Everything saved to disk as clips over every run of the app.
pub(super) fn saved(status: &Status) -> String {
    gigabytes(status.saved_bytes)
}

pub(super) fn capture(status: &Status) -> String {
    status
        .backend
        .map_or_else(|| NO_VALUE.to_owned(), |backend| backend.to_string())
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
    status
        .encode_rate
        .map_or_else(|| NO_VALUE.to_owned(), |rate| format!("{rate:.0} fps"))
}

pub(super) fn last_clip(path: Option<&Path>) -> String {
    path.map_or_else(|| NO_VALUE.to_owned(), |path| path.display().to_string())
}

pub(super) fn buffer_estimate(megabytes: u64) -> String {
    format!("Buffer memory at this bitrate: about {megabytes} MB")
}
