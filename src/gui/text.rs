//! Every string the window shows, log lines included.

use crate::{
    APP_NAME,
    config::{Codec, Memory},
    gpu::{Adapter, ProbeError},
    replay::Stream,
    service::{State, Status},
    sys::release::Release,
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
pub(super) const TAB_ABOUT: &str = "About";

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
pub(super) const REPLAY_LENGTH: &str = "Replay length (s)";
pub(super) const BITRATE: &str = "Bitrate (Mbps)";
pub(super) const HOTKEY: &str = "Save hotkey";
pub(super) const SCREENSHOT_HOTKEY: &str = "Screenshot key";
pub(super) const MODE: &str = "Record";
pub(super) const BACKEND: &str = "Backend";
pub(super) const FOLDER: &str = "Clip folder";
pub(super) const BROWSE: &str = "Browse…";
pub(super) const AUDIO: &str = "Record system sound";
pub(super) const MICROPHONE: &str = "Record the microphone";
pub(super) const CURSOR: &str = "Record the mouse cursor";
pub(super) const MEMORY: &str = "Buffer in";
pub(super) const APPLY: &str = "Apply";
pub(super) const NUMBER_REQUIRED: &str =
    "Enter whole numbers for the replay length, bitrate and frame rate.";
pub(super) const THEME: &str = "Theme";
pub(super) const START_WITH_WINDOWS: &str = "Start with Windows";
pub(super) const CLOSE_TO_TRAY: &str = "Close to tray";
pub(super) const CHECK_FOR_UPDATES: &str = "Check for updates";

pub(super) const GROUP_ABOUT: &str = "hindsight";
pub(super) const GROUP_DETAILS: &str = "Details";
pub(super) const GROUP_LINKS: &str = "Links";
pub(super) const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
pub(super) const BUILD: &str = "Build";
pub(super) const COMPILER: &str = "Compiler";
pub(super) const WINDOWS: &str = "Windows";
pub(super) const GPU: &str = "GPU";
pub(super) const LINK_WEBSITE: &str = "Website";
pub(super) const LINK_GITHUB: &str = "GitHub";
pub(super) const LINK_REPORT: &str = "Report a problem";
pub(super) const LINK_LOG_FOLDER: &str = "Open log folder";
pub(super) const GROUP_UPDATES: &str = "Updates";
pub(super) const UPDATE_CHECK: &str = "Check now";
pub(super) const UPDATE_INSTALL: &str = "Update";
pub(super) const UPDATE_DOWNLOAD: &str = "Download";
pub(super) const UPDATE_NOTES: &str = "What's new";
pub(super) const UPDATE_UNCHECKED: &str = "Not checked yet.";
pub(super) const UPDATE_CHECKING: &str = "Checking for updates…";
pub(super) const UPDATE_CURRENT: &str = "hindsight is up to date.";
pub(super) const UPDATE_DECLINED: &str = "Update · administrator prompt declined";
pub(super) const UPDATE_THREAD_MISSING: &str = "the update thread could not start";

pub(super) const STARTUP_ENABLED: &str = "Start with Windows · enabled";
pub(super) const STARTUP_DISABLED: &str = "Start with Windows · disabled";
pub(super) const QUIT_FROM_TRAY: &str = "Quit · from the tray menu";
pub(super) const CLOSED_TO_TRAY: &str = "Window closed · recording continues from the tray";
pub(super) const QUIT_ON_CLOSE: &str = "Quit · window closed";
pub(super) const FOREGROUND_WATCH_FAILED: &str =
    "Foreground hook · not installed · apps noticed every second";

pub(super) const STARTUP_FAILED_TITLE: &str = "hindsight could not start";
pub(super) const STARTUP_FAILED_HELP: &str =
    "Close this window, resolve the error, then launch again.";

pub(super) fn tray_unavailable(error: impl Display) -> String {
    format!("Tray · unavailable ({error}) · closing the window quits")
}

pub(super) fn startup_setting_unreadable(error: impl Display) -> String {
    format!("Start with Windows · entry unreadable ({error})")
}

pub(super) fn startup_setting_failed(error: impl Display) -> String {
    format!("Start with Windows · not updated ({error})")
}

pub(super) fn thumbnails_unavailable(error: impl Display) -> String {
    format!("Clips · no thumbnails ({error})")
}

pub(super) fn thumbnail_failed(path: &Path, error: impl Display) -> String {
    format!("Clips · no thumbnail for {} ({error})", file_name(path))
}

pub(super) fn clip_not_listed(path: &Path, error: impl Display) -> String {
    format!("Clips · {} not listed ({error})", file_name(path))
}

pub(super) fn folder_pick_failed(error: impl Display) -> String {
    format!("Clip folder · dialog failed ({error})")
}

pub(super) fn clip_open_failed(path: &Path, error: impl Display) -> String {
    format!("Could not open {} · {error}", file_name(path))
}

pub(super) fn clip_reveal_failed(path: &Path, error: impl Display) -> String {
    format!("Could not show {} in Explorer · {error}", file_name(path))
}

pub(super) fn clip_deleted(path: &Path) -> String {
    format!("Recycled · {}", file_name(path))
}

pub(super) fn clip_delete_failed(path: &Path, error: impl Display) -> String {
    format!("Could not recycle {} · {error}", file_name(path))
}

pub(super) fn clip_menu_failed(error: impl Display) -> String {
    format!("Clip menu · not shown ({error})")
}

pub(super) fn clip_caption(title: &str, bytes: u64) -> String {
    // A no-break space, because the list view wraps captions and would split the size from its
    // unit.
    format!("{title} {}", format_bytes(bytes).replace(' ', "\u{a0}"))
}

pub(super) fn status(status: &Status) -> String {
    let message = match status.state {
        State::Starting(_) => "Starting capture…",
        // The last app's buffer is kept until the next app, so a save still has content.
        State::Waiting if holds_frames(status) => {
            "Waiting for a full-screen app · the last buffer stays saveable"
        }
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

/// The source and the encoder are shown only while capture runs. Between apps the buffer is kept,
/// but the lines would otherwise suggest that the last app was still being recorded.
pub(super) fn source(status: &Status) -> String {
    status
        .state
        .running()
        .and_then(|_| status.source.clone())
        .unwrap_or_else(|| NO_VALUE.to_owned())
}

fn holds_frames(status: &Status) -> bool {
    status.buffer.is_some_and(|buffer| buffer.held.frames > 0)
}

pub(super) fn backend(status: &Status) -> String {
    status.state.running().map_or_else(
        || NO_VALUE.to_owned(),
        |running| running.backend.to_string(),
    )
}

pub(super) fn encoder(status: &Status) -> String {
    status
        .state
        .running()
        .and_then(|_| stream(status))
        .map_or_else(
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

/// The span the next save would cover and the memory it takes, as "27 s of 30 s · 327 MB".
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

/// Where the buffer is kept and how much it reserves, as "GPU memory (VRAM) · about 3.8 GB".
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

/// Formats every size the window shows: "312 MB" below a gigabyte and "1.5 GB" from there, in
/// decimal units.
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
    format!("Codecs · {}", names.join(", "))
}

pub(super) fn codecs_unknown(error: impl Display) -> String {
    format!("Codecs · unknown ({error}) · offering all")
}

pub(super) fn codec_unsupported(codec: &str) -> String {
    format!(
        "This GPU's encoder does not support {codec}; choose another codec in Settings and apply"
    )
}

pub(super) fn name_and_version() -> String {
    format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))
}

/// The commit built and the day it was built, as "1a2b3c4 · 2026-10-14".
pub(super) fn build(commit: &str, date: &str) -> String {
    format!("{commit} · {date}")
}

/// The compiler's name and version, as "rustc 1.98.0", taken from `rustc --version`, which
/// continues with the compiler's own commit.
pub(super) fn compiler(version: &str) -> String {
    let mut words = version.split_whitespace();
    match (words.next(), words.next()) {
        (Some(name), Some(number)) => format!("{name} {number}"),
        _ => version.to_owned(),
    }
}

/// The Windows release, as "Windows 11 25H2 · build 26200.6899". Windows 11 is distinguished only
/// by its build number.
pub(super) fn windows_release(release: &Release) -> String {
    const FIRST_WINDOWS_11_BUILD: u32 = 22_000;
    let generation = match release.build {
        Some(build) if build >= FIRST_WINDOWS_11_BUILD => "Windows 11",
        Some(_) => "Windows 10",
        None => "Windows",
    };
    let mut line = generation.to_owned();
    if let Some(version) = &release.display_version {
        line = format!("{line} {version}");
    }
    match (release.build, release.revision) {
        (Some(build), Some(revision)) => format!("{line} · build {build}.{revision}"),
        (Some(build), None) => format!("{line} · build {build}"),
        _ => line,
    }
}

/// The GPU and its driver, as "NVIDIA GeForce RTX 4080 · driver 581.57", or why there is no such
/// GPU.
pub(super) fn gpu(adapter: &Result<Adapter, ProbeError>) -> String {
    match adapter {
        Ok(Adapter {
            name,
            driver: Some(driver),
        }) => format!("{name} · driver {driver}"),
        Ok(Adapter { name, driver: None }) => name.clone(),
        Err(error) => capitalised(&error.to_string()),
    }
}

pub(super) fn update_available(version: impl Display) -> String {
    format!("hindsight {version} is available.")
}

pub(super) fn update_downloading(version: impl Display) -> String {
    format!("Downloading hindsight {version}…")
}

pub(super) fn update_check_failed(error: impl Display) -> String {
    format!("Could not check for updates: {error}.")
}

pub(super) fn update_failed(error: impl Display) -> String {
    format!("Could not update: {error}.")
}

pub(super) fn update_confirmation(version: impl Display) -> String {
    format!(
        "hindsight closes to install {version} and starts again when it is done. The replay \
         buffer is discarded, so save a clip first if you want one."
    )
}

pub(super) fn updates_unavailable(error: impl Display) -> String {
    format!("Update · unavailable ({error})")
}

pub(super) fn update_found(version: impl Display) -> String {
    format!("Update · hindsight {version} available")
}

pub(super) fn update_check_log(error: impl Display) -> String {
    format!("Update · check failed ({error})")
}

pub(super) fn update_download_log(version: impl Display, error: impl Display) -> String {
    format!("Update · hindsight {version} not installed ({error})")
}

pub(super) fn update_installing(version: impl Display) -> String {
    format!("Update · installing hindsight {version} · quitting")
}

pub(super) fn link_open_failed(target: impl Display, error: impl Display) -> String {
    format!("Could not open {target} · {error}")
}

fn capitalised(message: &str) -> String {
    let mut characters = message.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Backend, Memory},
        replay::Held,
        service::{Buffer, Running},
    };

    #[test]
    fn between_apps_the_source_and_encoder_are_dashes_and_a_kept_buffer_is_said() {
        let mut shown = Status {
            state: State::Waiting,
            source: Some("cs2.exe".to_owned()),
            ..Status::default()
        };
        assert_eq!(source(&shown), "—");
        assert_eq!(encoder(&shown), "—");
        assert_eq!(status(&shown), "●  Waiting for a full-screen app");
        shown.buffer = Some(Buffer {
            stream: None,
            held: Held {
                frames: 240,
                bytes: 1_000,
                stored: true,
            },
            memory: Memory::Ram,
        });
        assert_eq!(
            status(&shown),
            "●  Waiting for a full-screen app · the last buffer stays saveable"
        );
        shown.state = State::Recording(Running {
            backend: Backend::Nvfbc,
            encode_rate: None,
        });
        assert_eq!(source(&shown), "cs2.exe");
    }

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
    fn the_compiler_line_keeps_the_name_and_number_and_passes_anything_else_through() {
        assert_eq!(
            compiler("rustc 1.98.0 (6b00bc388 2026-09-15)"),
            "rustc 1.98.0"
        );
        assert_eq!(compiler("unknown"), "unknown");
        assert_eq!(
            build("1a2b3c4-dirty", "2026-10-14"),
            "1a2b3c4-dirty · 2026-10-14"
        );
    }

    #[test]
    fn the_windows_line_tells_11_from_10_by_build_and_says_what_it_knows() {
        let release = Release {
            display_version: Some("25H2".to_owned()),
            build: Some(26200),
            revision: Some(6899),
        };
        assert_eq!(
            windows_release(&release),
            "Windows 11 25H2 · build 26200.6899"
        );
        let older = Release {
            display_version: None,
            build: Some(17763),
            revision: None,
        };
        assert_eq!(windows_release(&older), "Windows 10 · build 17763");
        assert_eq!(windows_release(&Release::default()), "Windows");
    }

    #[test]
    fn the_gpu_line_adds_the_driver_when_known() {
        let mut adapter = Adapter {
            name: "NVIDIA GeForce RTX 4080".to_owned(),
            driver: Some("581.57".to_owned()),
        };
        assert_eq!(
            gpu(&Ok(adapter.clone())),
            "NVIDIA GeForce RTX 4080 · driver 581.57"
        );
        adapter.driver = None;
        assert_eq!(gpu(&Ok(adapter)), "NVIDIA GeForce RTX 4080");
        assert_eq!(capitalised("no NVIDIA GPU"), "No NVIDIA GPU");
        assert_eq!(capitalised(""), "");
    }

    #[test]
    fn the_name_line_carries_the_package_version() {
        assert_eq!(
            name_and_version(),
            format!("hindsight {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn the_hourly_write_volume_is_the_bitrate_for_an_hour() {
        assert_eq!(bytes_an_hour(100), 45_000_000_000);
        assert_eq!(bytes_an_hour(5), 2_250_000_000);
        assert_eq!(bytes_an_hour(u32::MAX), 1_932_735_282_750_000_000);
    }
}
