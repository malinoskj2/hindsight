//! Capture of a monitor or a window into the encoder's input texture. Each backend is a
//! [`Source`]; the pipeline drives whichever one the settings choose without knowing which:
//!
//! - [`wgc`], Windows Graphics Capture, records a monitor or a single window.
//! - [`dxgi_dd`], DXGI Desktop Duplication, records whole monitors only; a window target is
//!   recorded through the monitor it fills.
//! - [`nvfbc`], NVIDIA Frame Buffer Capture, records whole monitors like Desktop Duplication,
//!   cursor included, for licensed vendors holding the key NVIDIA issues.

mod dxgi_dd;
mod nvfbc;
mod wgc;

use super::device::Device;
use crate::{config::Backend, service::Target};
use std::{ffi::c_void, time::Duration};
use thiserror::Error;
use windows::Win32::{
    Foundation::POINT,
    Graphics::{
        Direct3D11::ID3D11Texture2D,
        Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint},
    },
};

#[derive(Debug, Error)]
pub(crate) enum CaptureError {
    /// This machine or display cannot be captured this way; trying again would fail the same
    /// way.
    #[error("{0}")]
    Unsupported(String),
    #[error("the captured display or window went away")]
    Closed,
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
    #[error("{0} failed with {1}")]
    Nvfbc(&'static str, nvfbc::Status),
}

/// What a tick found.
pub(super) enum Frame {
    /// A new picture was copied into the target.
    Fresh,
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
}

/// Starts capturing `target` with `backend`, delivering frames no faster than about twice per
/// `period` where the backend can be asked to.
pub(super) fn start(
    backend: Backend,
    device: &Device,
    period: Duration,
    target: &Target,
) -> Result<Box<dyn Source>, CaptureError> {
    Ok(match backend {
        Backend::Wgc => Box::new(wgc::Capture::start(device, period, target)?),
        Backend::Dxgi => Box::new(dxgi_dd::DesktopDuplication::start(device, target)?),
        Backend::Nvfbc => Box::new(nvfbc::Nvfbc::start(device, target)?),
    })
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
