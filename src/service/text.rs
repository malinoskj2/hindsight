//! The worker's log lines, some of which the Status tab also shows.
//!
//! Every line fits one row of the Logs tab: a short subject in sentence case, then lower-case
//! details joined by " · ", as in "Recording · AV1 1920×1080 · 240 fps". A cause goes in
//! parentheses after the detail it explains, and no line ends in a full stop. A fragment that
//! completes another line, such as the reason after "Capture stopped · ", starts in lower case.
//! Paths show only the file name, since the folder is in the settings. Lines with nothing to
//! fill in are constants. The others are formatted only when they are written.

use super::Target;
use crate::{
    config::{Backend, Config},
    replay::Stream,
};
use std::{
    fmt::{self, Display},
    path::Path,
};

/// The file name of `path`, or the whole path when it has none.
fn file_name(path: &Path) -> impl Display {
    path.file_name().unwrap_or(path.as_os_str()).display()
}

pub(super) fn settings_applied(config: &Config) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Settings · {} · {} · {} · {} fps · {} Mbps · {} s {} · {} · screenshot {}",
            config.mode,
            config.backend,
            config.codec,
            config.frame_rate,
            config.bitrate_mbps,
            config.buffer_seconds,
            config.buffer_memory.place(),
            config.hotkey,
            config.screenshot_hotkey
        )
    })
}

pub(super) fn save_settings_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Settings · not saved ({error})"))
}

pub(super) const RESTARTING: &str = "Restarting capture";
pub(super) const PAUSED: &str = "Paused · the buffer stays saveable";
pub(super) const RESUMED: &str = "Resumed";
pub(super) const RESIZED: &str = "Resized · capture restarted, buffer starts over";
pub(super) const DISPLAY_GONE: &str = "display gone";
pub(super) const NO_FRAMES: &str = "no frames from the display";
pub(super) const NO_PANIC_MESSAGE: &str = "no message";
pub(super) const WORKER_GONE: &str = "Worker stopped · request dropped";

pub(super) fn capture_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "capture thread panicked · {message}"))
}

pub(super) fn worker_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Worker panicked · {message} · start hindsight again"))
}

/// Names the window as well as the app, so a false detection can be traced to its window.
pub(super) fn target_found(target: &Target) -> impl Display {
    fmt::from_fn(move |f| match target {
        Target::Window {
            name,
            title,
            size: (width, height),
            ..
        } => write!(f, "Capturing · {name} · \"{title}\" · {width}×{height}"),
        Target::Monitor { name, .. } => write!(f, "Capturing · {name}"),
    })
}

pub(super) fn passed_over(name: &str, reason: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "{name} not recorded · {reason} · waiting for the next app"
        )
    })
}

pub(super) fn rotated_monitor(backend: Backend) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "its monitor is rotated, which {} does not undo; choose Windows Graphics Capture",
            backend.name()
        )
    })
}

pub(super) fn target_closed(name: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "{name} closed · buffer kept for the next app"))
}

pub(super) fn falling_back_to_monitor(name: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Capturing · {name}'s monitor (window not capturable)"))
}

pub(super) fn recording(stream: &Stream, config: &Config) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Recording · {} {}×{} · {} fps · {} Mbps · {} s",
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
    fmt::from_fn(move |f| write!(f, "Capture stopped · {error}"))
}

pub(super) fn capture_thread_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "capture thread did not start ({error})"))
}

pub(crate) const NOTHING_TO_SAVE: &str = "Nothing to save yet · capture has not started";
pub(super) const ALREADY_SAVING: &str = "Save · already in progress";

pub(super) fn save_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "save thread panicked · {message}"))
}

pub(super) fn clip_saved(seconds: f64, path: &Path) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Saved · {seconds:.0} s · {}", file_name(path)))
}

pub(super) fn saved_without_sound(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Saved without sound · {error}"))
}

pub(super) fn save_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Save failed · {error}"))
}

pub(super) const NOTHING_TO_SHOOT: &str =
    "Screenshot · nothing to take yet · capture has not started";
pub(super) const ALREADY_SHOOTING: &str = "Screenshot · already in progress";

pub(super) fn screenshot_saved(path: &Path) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Screenshot · {}", file_name(path)))
}

pub(super) fn screenshot_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Screenshot failed · {error}"))
}

pub(super) fn screenshot_panicked(message: &str) -> impl Display {
    fmt::from_fn(move |f| write!(f, "screenshot thread panicked · {message}"))
}

#[cfg(windows)]
pub(super) fn background_save_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Save · background mode refused ({error}) · normal priority"
        )
    })
}

pub(super) fn clip_size_unreadable(path: &Path, error: impl Display) -> impl Display {
    fmt::from_fn(move |f| {
        write!(
            f,
            "Totals · size of {} unreadable ({error}) · counted as 0",
            file_name(path)
        )
    })
}

pub(super) fn load_totals_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Totals · not read ({error}) · not kept"))
}

pub(super) fn save_totals_failed(error: impl Display) -> impl Display {
    fmt::from_fn(move |f| write!(f, "Totals · not saved ({error})"))
}
