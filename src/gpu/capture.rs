//! Capture of a monitor or a window into the encoder's input texture. Each backend is a
//! [`Source`]; the pipeline drives whichever one the settings choose without knowing which:
//!
//! - [`wgc`], Windows Graphics Capture, records a monitor or a single window.
//! - [`dxgi_dd`], DXGI Desktop Duplication, records whole monitors only; a window target is
//!   recorded through the monitor it fills.
//! - [`nvfbc`], NVIDIA Frame Buffer Capture, records whole monitors like Desktop Duplication,
//!   cursor included when asked for, for licensed vendors holding the key NVIDIA issues.

mod dxgi_dd;
mod nvfbc;
mod wgc;

use super::device::Device;
use crate::{config::Backend, service::Target};
use std::{
    ffi::c_void,
    time::{Duration, Instant},
};
use thiserror::Error;
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::{
        Direct3D11::ID3D11Texture2D,
        Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint},
    },
    UI::WindowsAndMessaging::IsWindow,
};

/// How often a backend that lost its capture to Windows or the driver tries to open it again
/// while it is refused.
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);

/// Why a capture could not start, or why a running one stopped.
#[derive(Debug, Error)]
pub(crate) enum CaptureError {
    /// This machine or display cannot be captured this way; trying again would fail the same
    /// way.
    #[error("{0}")]
    Unsupported(String),
    /// The captured monitor or window is gone.
    #[error("the captured display or window went away")]
    Closed,
    /// A Windows call failed in the named step.
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
    /// The NvFBC driver failed in the named step.
    #[error("{0} failed with {1}")]
    Nvfbc(&'static str, nvfbc::Status),
}

/// What a tick found.
pub(super) enum Frame {
    /// A new picture was copied into the target.
    Fresh,
    /// A new picture is in the source's own surface with this index, for the encoder to read
    /// in place; nothing was copied. Only after [`Source::hand_over_surfaces`].
    InSurface(usize),
    /// The screen has not changed since the last tick.
    Unchanged,
    /// The monitor's resolution, or the window's size, changed.
    Resized,
}

/// A running capture of one target.
pub(super) trait Source {
    /// The captured monitor's or window's size in pixels.
    fn size(&self) -> (u32, u32);

    /// Whether this captures a single window, which may never deliver a frame: Windows cannot
    /// compose an app in exclusive full screen.
    fn captures_window(&self) -> bool;

    /// Copies the newest captured frame into the top-left `width` × `height` of `target`, if
    /// one arrived since the last call.
    fn take(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError>;

    /// Switches the source, if it captures into textures of its own the size of
    /// [`size`](Self::size), to leaving each new picture where it is and reporting it as
    /// [`Frame::InSurface`] instead of copying it into the target; reports whether it did.
    fn hand_over_surfaces(&mut self) -> bool {
        false
    }

    /// The surface a [`Frame::InSurface`] named, which stays the same texture for the session.
    fn surface(&self, _index: usize) -> Option<ID3D11Texture2D> {
        None
    }

    /// The surface the next [`take`](Self::take) may capture into, for a source that chooses
    /// it, which the encoder must have finished reading first.
    fn next_surface(&self) -> Option<usize> {
        None
    }

    /// Gives the surfaces the encoder no longer needs, those for which `in_use` is false, back
    /// to a source that holds each until it is told so.
    fn release_surfaces(&mut self, _in_use: &dyn Fn(usize) -> bool) {}
}

/// Paces a backend's attempts to open its capture again after Windows or the driver took it
/// away, so a refusal is retried every [`REOPEN_INTERVAL`] rather than on every tick.
#[derive(Default)]
struct ReopenThrottle {
    /// When the next attempt is due, after a refusal.
    due: Option<Instant>,
}

impl ReopenThrottle {
    /// The current time if an attempt is due now, `None` while waiting out a refusal.
    fn attempt(&self) -> Option<Instant> {
        let now = Instant::now();
        self.due.is_none_or(|due| now >= due).then_some(now)
    }

    /// Lets the next attempt, after a successful one, go ahead at once.
    fn succeeded(&mut self) {
        self.due = None;
    }

    /// Holds the next attempt back after the one started at `now` was refused.
    fn refused(&mut self, now: Instant) {
        self.due = Some(now + REOPEN_INTERVAL);
    }
}

/// Starts capturing `target` with `backend`, delivering frames no faster than about twice per
/// `period` where the backend can be asked to, with the mouse cursor drawn in when `cursor`
/// is set and the backend can (Desktop Duplication never draws it).
pub(super) fn start(
    backend: Backend,
    device: &Device,
    period: Duration,
    target: &Target,
    cursor: bool,
) -> Result<Box<dyn Source>, CaptureError> {
    Ok(match backend {
        Backend::Wgc => Box::new(wgc::Capture::start(device, period, target, cursor)?),
        Backend::Dxgi => Box::new(dxgi_dd::Capture::start(device, target)?),
        Backend::Nvfbc => Box::new(nvfbc::Capture::start(device, target, cursor)?),
    })
}

/// Wraps a Windows error with the step that failed, for `map_err`.
fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |error| CaptureError::Windows(step, error)
}

/// The monitor a whole-monitor backend records for `target`, and the window it stands in for
/// when the target is one.
fn monitor_of(target: &Target) -> (HMONITOR, Option<HWND>) {
    match *target {
        Target::Monitor { handle, .. } => (monitor(handle), None),
        Target::Window {
            window,
            monitor: handle,
            ..
        } => (monitor(handle), Some(HWND(window as *mut c_void))),
    }
}

/// The monitor a target's handle names; 0 is the primary one.
fn monitor(handle: isize) -> HMONITOR {
    if handle == 0 {
        // SAFETY: (0, 0) always lies on the primary monitor.
        unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
    } else {
        HMONITOR(handle as *mut c_void)
    }
}

/// Whether the window a whole-monitor backend records through its monitor has been destroyed.
fn is_window_closed(window: Option<HWND>) -> bool {
    // SAFETY: IsWindow accepts any handle, live or not.
    window.is_some_and(|window| !unsafe { IsWindow(Some(window)) }.as_bool())
}
