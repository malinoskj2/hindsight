//! Every string the window shows, log lines included.

use crate::{
    config::{Codec, Memory},
    service::{DisplayStatus, Status},
};
use std::{fmt::Display, path::Path};

pub(super) use crate::service::text::NOTHING_TO_SAVE;

const NO_VALUE: &str = "—";
const GIGABYTE: f64 = 1_073_741_824.0;
const MEGABYTE: f64 = 1_048_576.0;

pub(super) const TAB_STATUS: &str = "Status";
pub(super) const TAB_CLIPS: &str = "Clips";
pub(super) const TAB_SETTINGS: &str = "Settings";
pub(super) const TAB_LOGS: &str = "Logs";

pub(super) const GROUP_RECORDING: &str = "Recording";
pub(super) const GROUP_STATS: &str = "Stats";
pub(super) const GROUP_BUFFER: &str = "Buffer";
pub(super) const BUFFER_HELD: &str = "Held";
pub(super) const BUFFER_PLACE: &str = "Kept in";
pub(super) const GROUP_CONTROLS: &str = "Controls";
pub(super) const CONTROL_SAVE: &str = "Save";
pub(super) const CONTROL_PAUSE: &str = "Pause";
pub(super) const CONTROL_RESUME: &str = "Resume";
pub(super) const CONTROL_RESTART: &str = "Restart";
pub(super) const CONTROL_OPEN_CLIP: &str = "Open clip";
pub(super) const CONTROL_OPEN_FOLDER: &str = "Open folder";
pub(super) const GROUP_GENERAL: &str = "General";

pub(super) const SOURCE: &str = "Source";
pub(super) const ENCODER: &str = "Encoder";
pub(super) const SESSION_WRITTEN: &str = "Session Written";
pub(super) const SESSION_SAVED: &str = "Session Saved";
pub(super) const LIFETIME_WRITTEN: &str = "Lifetime Written";
pub(super) const LIFETIME_SAVED: &str = "Lifetime Saved";

pub(super) const NO_CLIPS: &str =
    "No clips yet — press the hotkey while a full-screen app is running";
pub(super) const CLIP_MENU_OPEN: &str = "&Open";
pub(super) const CLIP_MENU_REVEAL: &str = "Show in &Explorer";
pub(super) const CLIP_MENU_DELETE: &str = "&Delete";

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

pub(super) fn clip_reveal_failed(path: &Path, error: impl Display) -> String {
    format!("Could not show {} in Explorer: {error}", path.display())
}

pub(super) fn clip_deleted(path: &Path) -> String {
    format!("Moved {} to the Recycle Bin", path.display())
}

pub(super) fn clip_delete_failed(path: &Path, error: impl Display) -> String {
    format!(
        "Could not move {} to the Recycle Bin: {error}",
        path.display()
    )
}

pub(super) fn clip_menu_failed(error: impl Display) -> String {
    format!("The clip menu could not be shown: {error}")
}

pub(super) fn clip_caption(title: &str, bytes: u64) -> String {
    // No-break space: the list view wraps captions and would split the size from its unit.
    format!("{title} {:.0}\u{a0}MB", bytes as f64 / MEGABYTE)
}

pub(super) fn status(state: DisplayStatus) -> String {
    let message = match state {
        DisplayStatus::Starting => "Starting capture…",
        DisplayStatus::Waiting => "Waiting for a full-screen app",
        DisplayStatus::Recording => "Recording to the replay buffer",
        DisplayStatus::Paused => "Paused · the buffer stays saveable",
        DisplayStatus::Retrying => "Capture stopped · retrying shortly",
        DisplayStatus::Unsupported => "Cannot record with these settings",
    };
    format!("●  {message}")
}

pub(super) fn source(status: &Status) -> String {
    status.source.clone().unwrap_or_else(|| NO_VALUE.to_owned())
}

pub(super) fn backend(status: &Status) -> String {
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

/// "27 s of 30 s · 312 MB": the span the next save would cover and what it takes.
pub(super) fn buffer_held(status: &Status) -> String {
    let Some(stream) = status.stream else {
        return NO_VALUE.to_owned();
    };
    let held = status.buffer_held;
    let seconds = held.frames as f64 / f64::from(stream.frame_rate.max(1));
    format!(
        "{seconds:.0} s of {} s · {}",
        status.buffer_seconds,
        megabytes(held.bytes)
    )
}

/// "GPU memory (VRAM) · about 3,648 MB": where the buffer is kept and what it reserves.
pub(super) fn buffer_place(status: &Status) -> String {
    status.buffer_memory.map_or_else(
        || NO_VALUE.to_owned(),
        |memory| {
            format!(
                "{} · about {}",
                memory.name(),
                megabytes(status.buffer_capacity_bytes)
            )
        },
    )
}

fn megabytes(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / MEGABYTE)
}

pub(super) fn session_written(status: &Status) -> String {
    gigabytes(status.session_written_bytes)
}

pub(super) fn session_saved(status: &Status) -> String {
    gigabytes(status.session_saved_bytes)
}

pub(super) fn lifetime_written(status: &Status) -> String {
    gigabytes_or_dash(status.buffered_lifetime_bytes)
}

pub(super) fn lifetime_saved(status: &Status) -> String {
    gigabytes_or_dash(status.saved_bytes)
}

pub(super) fn buffer_estimate(megabytes: u64, memory: Memory, bitrate_mbps: u32) -> String {
    let place = memory.place();
    match memory {
        Memory::Ram => format!("Buffer {place} at this bitrate: about {megabytes} MB"),
        Memory::Vram => format!(
            "Buffer {place} at this bitrate: about {megabytes} MB, plus as much RAM while a clip is saved"
        ),
        Memory::Disk => format!(
            "Buffer {place} at this bitrate: about {megabytes} MB, writing about {} GB an hour",
            gigabytes_an_hour(bitrate_mbps)
        ),
    }
}

fn gigabytes_an_hour(bitrate_mbps: u32) -> u64 {
    const SECONDS_AN_HOUR: u64 = 3600;
    const BITS_PER_BYTE: u64 = 8;
    const MEGABYTES_PER_GIGABYTE: u64 = 1000;
    let megabytes = u64::from(bitrate_mbps) * SECONDS_AN_HOUR / BITS_PER_BYTE;
    (megabytes + MEGABYTES_PER_GIGABYTE / 2) / MEGABYTES_PER_GIGABYTE
}

fn gigabytes_or_dash(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| NO_VALUE.to_owned(), gigabytes)
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / GIGABYTE)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.display().to_string(),
    )
}

pub(super) fn codecs_offered(codecs: &[Codec]) -> String {
    let names = codecs.iter().map(|codec| codec.name()).collect::<Vec<_>>();
    format!("Codecs this GPU encodes: {}", names.join(", "))
}

pub(super) fn codecs_unknown(error: impl Display) -> String {
    format!("Could not ask the GPU which codecs it encodes, so every codec is offered: {error}")
}

pub(super) fn codec_unsupported(codec: &str) -> String {
    format!(
        "This GPU's encoder does not support {codec}; choose another codec in Settings and apply"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_shown_in_gigabytes_to_two_decimals_or_as_a_dash() {
        assert_eq!(gigabytes_or_dash(None), "—");
        assert_eq!(gigabytes_or_dash(Some(0)), "0.00 GB");
        assert_eq!(gigabytes(1_073_741_824), "1.00 GB");
        assert_eq!(gigabytes(1_610_612_736), "1.50 GB");
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
    fn the_buffer_rows_show_the_span_held_and_where_it_is_kept() {
        use crate::replay::{Held, Matrix, Stream};
        let mut status = Status::default();
        assert_eq!(buffer_held(&status), "—");
        assert_eq!(buffer_place(&status), "—");
        status.stream = Some(Stream {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            frame_rate: 60,
            matrix: Matrix::Bt709,
        });
        status.buffer_seconds = 30;
        status.buffer_held = Held {
            frames: 1620,
            bytes: 327_155_712,
            stored: true,
        };
        status.buffer_memory = Some(Memory::Vram);
        status.buffer_capacity_bytes = 3_825_205_248;
        assert_eq!(buffer_held(&status), "27 s of 30 s · 312 MB");
        assert_eq!(buffer_place(&status), "GPU memory (VRAM) · about 3648 MB");
    }

    #[test]
    fn the_buffer_estimate_names_where_the_buffer_lives() {
        assert_eq!(
            buffer_estimate(500, Memory::Ram, 50),
            "Buffer in RAM at this bitrate: about 500 MB"
        );
        assert!(
            buffer_estimate(500, Memory::Vram, 50)
                .ends_with("plus as much RAM while a clip is saved")
        );
        assert_eq!(
            buffer_estimate(1250, Memory::Disk, 100),
            "Buffer on disk at this bitrate: about 1250 MB, writing about 45 GB an hour"
        );
    }

    #[test]
    fn the_hourly_write_volume_is_the_bitrate_for_an_hour() {
        assert_eq!(gigabytes_an_hour(100), 45);
        assert_eq!(gigabytes_an_hour(50), 23);
        assert_eq!(gigabytes_an_hour(5), 2);
        assert_eq!(gigabytes_an_hour(150), 68);
        assert_eq!(gigabytes_an_hour(u32::MAX), 1_932_735_283);
    }
}
