//! Capture of a monitor or a window into the encoder's input texture. Each backend is a
//! [`Source`]; the pipeline drives whichever one the settings choose without knowing which:
//!
//! - [`wgc`], Windows Graphics Capture, records a monitor or a single window.
//! - [`dxgi_dd`], DXGI Desktop Duplication, records whole monitors only; a window target is
//!   recorded through the monitor it fills.
//! - [`nvfbc`], NVIDIA Frame Buffer Capture, records whole monitors like Desktop Duplication,
//!   cursor included when asked for, for licensed vendors holding the key NVIDIA issues. It
//!   grabs into Direct3D 9 surfaces, or, as [`nvfbc::CudaCapture`], into CUDA memory that an
//!   encoder on a CUDA context reads in place.

mod dxgi_dd;
mod nvfbc;
mod wgc;

use super::{
    cuda::{Context, CudaError},
    device::Device,
};
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
    /// The CUDA driver failed in the named step.
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    /// The driver ended the capture in a way only a fresh session mends; the encoder's state
    /// cannot be carried over.
    #[error("{0}")]
    Restart(&'static str),
}

/// A surface of a source's own, which the encoder reads in place once it is handed over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Surface {
    /// A texture on the shared Direct3D 11 device.
    Texture(ID3D11Texture2D),
    /// Rows of 32-bit BGRA in CUDA device memory, `pitch` bytes apart, which only an encoder
    /// on the source's CUDA context can read.
    CudaBuffer { pointer: u64, pitch: u32 },
}

/// The device the encoder must open its session on to read a source's surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EncoderDevice {
    /// The shared Direct3D 11 device, which also holds the encoder's own input textures.
    Direct3D,
    /// The CUDA context the source's buffers live in; the encoder then has no input textures
    /// of its own and reads the source's surfaces only. `primary` names the device whose
    /// primary context it is, when it is one: memory that must outlive the source (the replay
    /// buffer) can then be kept in it by retaining the context again.
    Cuda {
        context: Context,
        primary: Option<super::cuda::Device>,
    },
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
    /// one arrived since the last call. There is no target when the encoder runs on a CUDA
    /// context, which only the surfaces of a source that asked for such an encoder reach.
    fn take(
        &mut self,
        device: &Device,
        target: Option<&ID3D11Texture2D>,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError>;

    /// The device the encoder must run on to read this source's surfaces.
    fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Direct3D
    }

    /// Switches the source, if it captures into textures of its own the size of
    /// [`size`](Self::size), to leaving each new picture where it is and reporting it as
    /// [`Frame::InSurface`] instead of copying it into the target; reports whether it did.
    fn hand_over_surfaces(&mut self) -> bool {
        false
    }

    /// The surface a [`Frame::InSurface`] named, which stays the same memory for the session.
    fn surface(&self, _index: usize) -> Option<Surface> {
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
        Backend::NvfbcCuda => Box::new(nvfbc::CudaCapture::start(device, target, cursor)?),
    })
}

/// Wraps a Windows error with the step that failed, for `map_err`.
fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |error| CaptureError::Windows(step, error)
}

/// The texture a source that copies its frames was given to copy into: the pipeline passes
/// one whenever the encoder runs on the Direct3D device, the only encoder such a source
/// runs with.
fn target_of(target: Option<&ID3D11Texture2D>) -> &ID3D11Texture2D {
    target.expect("a source that copies frames runs with an encoder on the Direct3D device")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopen_is_attempted_at_once_until_refused() {
        let mut throttle = ReopenThrottle::default();
        let now = throttle
            .attempt()
            .expect("the first attempt is due at once");
        throttle.refused(now);
        assert!(
            throttle.attempt().is_none(),
            "a refusal holds the next attempt back"
        );
        throttle.succeeded();
        assert!(
            throttle.attempt().is_some(),
            "a success lets the next attempt go ahead"
        );
    }

    #[test]
    fn reopen_is_attempted_again_once_the_interval_has_passed() {
        let mut throttle = ReopenThrottle::default();
        let Some(refused_at) = Instant::now().checked_sub(REOPEN_INTERVAL) else {
            return;
        };
        throttle.refused(refused_at);
        assert!(throttle.attempt().is_some());
    }
}
