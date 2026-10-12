//! Everything the window shows in words, its own log lines included. Kept apart from the pages
//! so the layout reads without the wording, and the wording can change without the layout.

use crate::{
    config::Memory,
    service::{DisplayStatus, Status},
};
use std::{fmt::Display, path::Path};

/// Shown in place of a value that is not known yet.
const NO_VALUE: &str = "—";
/// Bytes in a gigabyte here: 1024 cubed.
const GIGABYTE: f64 = 1_073_741_824.0;
/// Bytes in a megabyte here: 1024 squared.
const MEGABYTE: f64 = 1_048_576.0;

pub(super) const TAB_STATUS: &str = "Status";
pub(super) const TAB_CLIPS: &str = "Clips";
pub(super) const TAB_SETTINGS: &str = "Settings";
pub(super) const TAB_LOGS: &str = "Logs";

pub(super) const GROUP_RECORDING: &str = "Recording";
pub(super) const GROUP_STATS: &str = "Stats";
pub(super) const GROUP_RECORDING_SETTINGS: &str = "Recording";
pub(super) const GROUP_GENERAL: &str = "General";

pub(super) const SOURCE: &str = "Source";
pub(super) const ENCODER: &str = "Encoder";
pub(super) const CAPTURE_RATE: &str = "Frame rate";
pub(super) const SESSION_WRITTEN: &str = "Session Written";
pub(super) const SESSION_SAVED: &str = "Session Saved";
pub(super) const LIFETIME_WRITTEN: &str = "Lifetime Written";
pub(super) const LIFETIME_SAVED: &str = "Lifetime Saved";

/// What the Clips tab shows in place of its list while the clip folder holds no clip.
pub(super) const NO_CLIPS: &str =
    "No clips yet — press the hotkey while a full-screen app is running";

pub(super) const CODEC: &str = "Codec";
pub(super) const FRAME_RATE: &str = "Frame rate";
pub(super) const BUFFER_SECONDS: &str = "Buffer (s)";
pub(super) const BITRATE: &str = "Bitrate (Mbps)";
pub(super) const HOTKEY: &str = "Save hotkey";
pub(super) const MODE: &str = "Record";
pub(super) const BACKEND: &str = "Backend";
pub(super) const FOLDER: &str = "Clip folder";
pub(super) const AUDIO: &str = "Record system sound (what the default output device plays)";
pub(super) const CURSOR: &str = "Record the mouse cursor (not with DXGI Desktop Duplication)";
pub(super) const MEMORY: &str = "Buffer in";
pub(super) const APPLY: &str = "Apply";
pub(super) const NUMBER_REQUIRED: &str =
    "Enter whole numbers for the frame rate, buffer and bitrate.";
pub(super) const THEME: &str = "Theme";
pub(super) const START_WITH_WINDOWS: &str = "Start with Windows";

pub(super) const STARTUP_ENABLED: &str = "Start with Windows enabled";
pub(super) const STARTUP_DISABLED: &str = "Start with Windows disabled";
pub(super) const QUIT_FROM_TRAY: &str = "Quit chosen from the tray menu";
pub(super) const CLOSED_TO_TRAY: &str = "Window closed; recording continues from the tray";
pub(super) const FOREGROUND_WATCH_FAILED: &str = "Full-screen apps are noticed every second rather than at once: the foreground hook could not be installed";

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

pub(super) fn startup_setting_unreadable(error: impl Display) -> String {
    format!("The Start with Windows entry could not be read: {error}")
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

pub(super) fn thumbnails_unavailable(error: impl Display) -> String {
    format!("Clips are listed without thumbnails: {error}")
}

pub(super) fn thumbnail_failed(path: &Path, error: impl Display) -> String {
    format!("No thumbnail for {}: {error}", file_name(path))
}

pub(super) fn clip_not_listed(path: &Path, error: impl Display) -> String {
    format!("{} could not be listed: {error}", file_name(path))
}

pub(super) fn clip_open_failed(path: &Path, error: impl Display) -> String {
    format!("Could not open {}: {error}", path.display())
}

/// The caption under a clip's thumbnail: when it was taken, or its name, and its size in
/// whole megabytes.
pub(super) fn clip_caption(title: &str, bytes: u64) -> String {
    // A no-break space keeps the size on one line: the list view wraps the caption, and
    // "760" alone at the end of a line reads as nothing.
    format!("{title} {:.0}\u{a0}MB", bytes as f64 / MEGABYTE)
}

/// The state line: what the recorder is doing, after a dot.
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

/// What is being captured, or a dash while it is not known.
pub(super) fn source(status: &Status) -> String {
    status.source.clone().unwrap_or_else(|| NO_VALUE.to_owned())
}

/// The capture backend in use, or a dash while it is not known.
pub(super) fn backend(status: &Status) -> String {
    status
        .backend
        .map_or_else(|| NO_VALUE.to_owned(), |backend| backend.to_string())
}

/// The codec, frame size and bitrate being encoded, or a dash while they are not known.
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

/// The rate frames are being encoded at, or a dash while it is not known.
pub(super) fn rates(status: &Status) -> String {
    status
        .encode_rate
        .map_or_else(|| NO_VALUE.to_owned(), |rate| format!("{rate:.0} fps"))
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

/// Everything saved as clips over every run of the app.
pub(super) fn lifetime_saved(status: &Status) -> String {
    gigabytes(status.saved_bytes)
}

/// The memory the replay buffer takes at the entered bitrate and length, where it is kept; a
/// buffer in GPU memory passes through system memory again when a clip is saved.
pub(super) fn buffer_estimate(megabytes: u64, memory: Memory) -> String {
    let name = memory.short_name();
    match memory {
        Memory::Ram => format!("Buffer in {name} at this bitrate: about {megabytes} MB"),
        Memory::Vram => format!(
            "Buffer in {name} at this bitrate: about {megabytes} MB, plus as much RAM while a clip is saved"
        ),
    }
}

/// A total in gigabytes, or a dash while it is not known. Shown to two decimals, far coarser
/// than the rounding of a byte count to `f64`.
fn gigabytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || NO_VALUE.to_owned(),
        |bytes| format!("{:.2} GB", bytes as f64 / GIGABYTE),
    )
}

/// The last part of `path`, as the journal names a file.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_shown_in_gigabytes_to_two_decimals_or_as_a_dash() {
        assert_eq!(gigabytes(None), "—");
        assert_eq!(gigabytes(Some(0)), "0.00 GB");
        assert_eq!(gigabytes(Some(1_073_741_824)), "1.00 GB");
        assert_eq!(gigabytes(Some(1_610_612_736)), "1.50 GB");
    }

    #[test]
    fn the_encoder_line_needs_a_stream_and_the_rate_line_rounds_to_whole_frames() {
        let mut status = Status {
            bitrate_mbps: 20,
            ..Status::default()
        };
        assert_eq!(encoder(&status), "—");
        assert_eq!(rates(&status), "—");
        status.encode_rate = Some(59.6);
        assert_eq!(rates(&status), "60 fps");
    }

    #[test]
    fn a_clip_caption_rounds_the_size_to_whole_megabytes() {
        assert_eq!(
            clip_caption("2026-10-12 21:14:05", 0),
            "2026-10-12 21:14:05 0\u{a0}MB"
        );
        assert_eq!(
            clip_caption("boss.mp4", 155_189_248),
            "boss.mp4 148\u{a0}MB"
        );
        assert_eq!(clip_caption("boss.mp4", 1_572_864), "boss.mp4 2\u{a0}MB");
    }

    #[test]
    fn the_buffer_estimate_names_where_the_buffer_lives() {
        assert_eq!(
            buffer_estimate(500, Memory::Ram),
            format!(
                "Buffer in {} at this bitrate: about 500 MB",
                Memory::Ram.short_name()
            )
        );
        assert!(
            buffer_estimate(500, Memory::Vram).ends_with("plus as much RAM while a clip is saved")
        );
    }
}
