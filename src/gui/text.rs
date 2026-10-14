//! Every string the window shows, log lines included.

use crate::{
    config::{Codec, Memory},
    replay::Stream,
    service::{State, Status},
};
use std::{fmt::Display, path::Path};

pub(super) use crate::service::text::NOTHING_TO_SAVE;

pub(super) const SOUND_FAILED: &str = "Sound is not being recorded; the Logs tab says why.";
pub(super) const CLIP_WITHOUT_SOUND: &str =
    "The last clip was saved without sound; the Logs tab says why.";

const NO_VALUE: &str = "—";
// Decimal units, as the bitrate's megabits are.
const GIGABYTE: u64 = 1_000_000_000;
const MEGABYTE: u64 = 1_000_000;

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
pub(super) const BROWSE: &str = "Browse…";
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

pub(super) fn folder_pick_failed(error: impl Display) -> String {
    format!("Could not open the folder dialog: {error}")
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
    format!("{title} {}", format_bytes(bytes).replace(' ', "\u{a0}"))
}

pub(super) fn status(state: &State) -> String {
    let message = match state {
        State::Starting(_) => "Starting capture…",
        State::Waiting => "Waiting for a full-screen app",
        State::Recording(_) => "Recording to the replay buffer",
        State::Paused => "Paused · the buffer stays saveable",
        State::Retrying => "Capture stopped · retrying shortly",
        State::Unsupported => "Cannot record with these settings",
        State::Idle => "Not recording",
        State::Stopped => "Recording stopped · start hindsight again",
    };
    format!("●  {message}")
}

pub(super) fn source(status: &Status) -> String {
    status.source.clone().unwrap_or_else(|| NO_VALUE.to_owned())
}

pub(super) fn backend(status: &Status) -> String {
    status.state.running().map_or_else(
        || NO_VALUE.to_owned(),
        |running| running.backend.to_string(),
    )
}

pub(super) fn encoder(status: &Status) -> String {
    stream(status).map_or_else(
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
        .state
        .running()
        .and_then(|running| running.encode_rate)
        .map_or_else(|| NO_VALUE.to_owned(), |rate| format!("{rate:.0} fps"))
}

/// "27 s of 30 s · 327 MB": the span the next save would cover and what it takes.
pub(super) fn buffer_held(status: &Status) -> String {
    let Some((stream, held)) = status
        .buffer
        .and_then(|buffer| Some((buffer.stream?, buffer.held)))
    else {
        return NO_VALUE.to_owned();
    };
    let seconds = held.frames as f64 / f64::from(stream.frame_rate.max(1));
    format!(
        "{seconds:.0} s of {} s · {}",
        status.buffer_seconds,
        format_bytes(held.bytes)
    )
}

/// "GPU memory (VRAM) · about 3.8 GB": where the buffer is kept and what it reserves.
pub(super) fn buffer_place(status: &Status) -> String {
    status.buffer.map_or_else(
        || NO_VALUE.to_owned(),
        |buffer| {
            format!(
                "{} · about {}",
                buffer.memory.name(),
                format_bytes(status.buffer_capacity_bytes)
            )
        },
    )
}

fn stream(status: &Status) -> Option<Stream> {
    status.buffer.and_then(|buffer| buffer.stream)
}

/// Every size the window shows: "312 MB" below a gigabyte, "1.5 GB" from there, in decimal units.
fn format_bytes(bytes: u64) -> String {
    // What would round up to 1000 MB is shown as 1.0 GB.
    if bytes < GIGABYTE - MEGABYTE / 2 {
        format!("{:.0} MB", bytes as f64 / MEGABYTE as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GIGABYTE as f64)
    }
}

pub(super) fn session_written(status: &Status) -> String {
    format_bytes(status.bytes.session_written)
}

pub(super) fn session_saved(status: &Status) -> String {
    format_bytes(status.bytes.session_saved)
}

pub(super) fn lifetime_written(status: &Status) -> String {
    bytes_or_dash(status.bytes.lifetime_written)
}

pub(super) fn lifetime_saved(status: &Status) -> String {
    bytes_or_dash(status.bytes.lifetime_saved)
}

pub(super) fn buffer_estimate(bytes: u64, memory: Memory, bitrate_mbps: u32) -> String {
    let place = memory.place();
    let size = format_bytes(bytes);
    match memory {
        Memory::Ram => format!("Buffer {place} at this bitrate: about {size}"),
        Memory::Vram => format!(
            "Buffer {place} at this bitrate: about {size}, plus as much RAM while a clip is saved"
        ),
        Memory::Disk => format!(
            "Buffer {place} at this bitrate: about {size}, writing about {} an hour",
            format_bytes(bytes_an_hour(bitrate_mbps))
        ),
    }
}

fn bytes_an_hour(bitrate_mbps: u32) -> u64 {
    const SECONDS_AN_HOUR: u64 = 3600;
    const BITS_PER_BYTE: u64 = 8;
    u64::from(bitrate_mbps) * MEGABYTE * SECONDS_AN_HOUR / BITS_PER_BYTE
}

fn bytes_or_dash(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| NO_VALUE.to_owned(), format_bytes)
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
    use crate::{config::Backend, service::Running};

    #[test]
    fn sizes_are_decimal_megabytes_below_a_gigabyte_and_tenths_of_one_above() {
        assert_eq!(format_bytes(0), "0 MB");
        assert_eq!(format_bytes(327_155_712), "327 MB");
        assert_eq!(format_bytes(999_499_999), "999 MB");
        assert_eq!(format_bytes(999_500_000), "1.0 GB");
        assert_eq!(format_bytes(1_550_000_000), "1.6 GB");
        assert_eq!(format_bytes(45_000_000_000), "45.0 GB");
        assert_eq!(bytes_or_dash(None), "—");
        assert_eq!(bytes_or_dash(Some(0)), "0 MB");
    }

    #[test]
    fn the_encoder_line_needs_a_stream_and_the_rate_line_rounds_to_whole_frames() {
        let mut status = Status {
            bitrate_mbps: 20,
            ..Status::default()
        };
        assert_eq!(encoder(&status), "—");
        assert_eq!(rates(&status), "—");
        status.state = State::Recording(Running {
            backend: Backend::Nvfbc,
            encode_rate: Some(59.6),
        });
        assert_eq!(rates(&status), "60 fps");
    }

    #[test]
    fn a_clip_caption_keeps_the_size_with_its_unit() {
        assert_eq!(
            clip_caption("2026-10-12 21:14:05", 0),
            "2026-10-12 21:14:05 0\u{a0}MB"
        );
        assert_eq!(
            clip_caption("boss.mp4", 155_189_248),
            "boss.mp4 155\u{a0}MB"
        );
        assert_eq!(clip_caption("boss.mp4", 1_572_864), "boss.mp4 2\u{a0}MB");
        assert_eq!(
            clip_caption("boss.mp4", 2_000_000_000),
            "boss.mp4 2.0\u{a0}GB"
        );
    }

    #[test]
    fn the_buffer_rows_show_the_span_held_and_where_it_is_kept() {
        use crate::{
            replay::{Held, Matrix},
            service::Buffer,
        };
        let mut status = Status::default();
        assert_eq!(buffer_held(&status), "—");
        assert_eq!(buffer_place(&status), "—");
        status.buffer = Some(Buffer {
            stream: Some(Stream {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                frame_rate: 60,
                matrix: Matrix::Bt709,
            }),
            held: Held {
                frames: 1620,
                bytes: 327_155_712,
                stored: true,
            },
            memory: Memory::Vram,
        });
        status.buffer_seconds = 30;
        status.buffer_capacity_bytes = 3_825_205_248;
        assert_eq!(buffer_held(&status), "27 s of 30 s · 327 MB");
        assert_eq!(buffer_place(&status), "GPU memory (VRAM) · about 3.8 GB");
    }

    #[test]
    fn the_buffer_estimate_names_where_the_buffer_lives() {
        assert_eq!(
            buffer_estimate(500_000_000, Memory::Ram, 50),
            "Buffer in RAM at this bitrate: about 500 MB"
        );
        assert!(
            buffer_estimate(500_000_000, Memory::Vram, 50)
                .ends_with("plus as much RAM while a clip is saved")
        );
        assert_eq!(
            buffer_estimate(1_260_000_000, Memory::Disk, 100),
            "Buffer on disk at this bitrate: about 1.3 GB, writing about 45.0 GB an hour"
        );
    }

    #[test]
    fn the_hourly_write_volume_is_the_bitrate_for_an_hour() {
        assert_eq!(bytes_an_hour(100), 45_000_000_000);
        assert_eq!(bytes_an_hour(5), 2_250_000_000);
        assert_eq!(bytes_an_hour(u32::MAX), 1_932_735_282_750_000_000);
    }
}
