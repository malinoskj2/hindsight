//! DXGI Desktop Duplication of a monitor. Windows writes each new desktop image into a surface
//! it shares with this process; on each tick the newest one, if any, is copied into the
//! encoder's input texture on the GPU and the surface handed straight back.
//!
//! Duplication sees whole monitors, and only those connected to the GPU it runs on. A window
//! target is recorded through the monitor it fills, so anything that comes in front of it is
//! recorded too, and the session ends when the window is destroyed. The cursor is not drawn
//! into the picture.
//!
//! Windows takes the duplication away whenever the desktop changes under it: a display mode
//! change, or the secure desktop of a UAC prompt or the lock screen. It is opened again in
//! place, keeping the buffer, unless the monitor's size changed.

use super::{CaptureError, Frame, REOPEN_INTERVAL, Source, failed, monitor_of, window_closed};
use crate::{gpu::device::Device, service::Target};
use std::time::Instant;
use windows::{
    Win32::{
        Foundation::{E_ACCESSDENIED, HWND},
        Graphics::{
            Direct3D11::ID3D11Texture2D,
            Dxgi::{
                Common::{DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED},
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND,
                DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutputDuplication,
                IDXGIResource,
            },
            Gdi::HMONITOR,
        },
    },
    core::{HRESULT, Interface},
};

/// Refusals that pass: the secure desktop is up, too many apps duplicate the monitor, the
/// session is being switched, or the desktop is still changing.
const PASSING_REFUSALS: [HRESULT; 4] = [
    E_ACCESSDENIED,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_ACCESS_LOST,
];

pub(super) struct DesktopDuplication {
    output: IDXGIOutput1,
    /// `None` while Windows has taken it away.
    duplication: Option<IDXGIOutputDuplication>,
    size: (u32, u32),
    /// The window whose monitor is recorded, when the target is one.
    window: Option<HWND>,
    /// Whether the current duplication has delivered a picture; its first one is always new.
    primed: bool,
    /// When a lost duplication is tried again.
    reopen_at: Option<Instant>,
}

impl DesktopDuplication {
    pub(super) fn start(device: &Device, target: &Target) -> Result<Self, CaptureError> {
        let (monitor, window) = monitor_of(target);
        let output = find_output(device, monitor)?;
        let (duplication, description) =
            duplicate(&output, device).map_err(first_duplication_failed)?;
        if is_rotated(&description) {
            return Err(CaptureError::Unsupported(
                "the monitor is rotated, which Desktop Duplication does not undo; choose Windows Graphics Capture"
                    .to_owned(),
            ));
        }

        Ok(Self {
            output,
            duplication: Some(duplication),
            size: size(&description),
            window,
            primed: false,
            reopen_at: None,
        })
    }

    /// Opens the duplication again after Windows took it away. While Windows refuses, the
    /// last picture is repeated; a new size or orientation ends the session.
    fn reopen(&mut self, device: &Device) -> Result<Frame, CaptureError> {
        let now = Instant::now();
        if self.reopen_at.is_some_and(|due| now < due) {
            return Ok(Frame::Unchanged);
        }
        let (duplication, description) = match duplicate(&self.output, device) {
            Ok(opened) => opened,
            Err(error) if PASSING_REFUSALS.contains(&error.code()) => {
                return self.retry_later(now);
            }
            Err(error) => return Err(failed("duplicate the monitor again")(error)),
        };
        self.reopen_at = None;
        if size(&description) != self.size || is_rotated(&description) {
            return Ok(Frame::Resized);
        }
        self.duplication = Some(duplication);
        self.primed = false;
        Ok(Frame::Unchanged)
    }

    /// Schedules the next attempt after Windows refused to duplicate the monitor for now,
    /// unless the monitor has left the desktop, which ends the session.
    fn retry_later(&mut self, now: Instant) -> Result<Frame, CaptureError> {
        // SAFETY: reads the description of a live output.
        let attached = unsafe { self.output.GetDesc() }
            .is_ok_and(|description| description.AttachedToDesktop.as_bool());
        if !attached {
            return Err(CaptureError::Closed);
        }
        self.reopen_at = Some(now + REOPEN_INTERVAL);
        Ok(Frame::Unchanged)
    }
}

impl Source for DesktopDuplication {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn captures_window(&self) -> bool {
        false
    }

    fn take(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        if window_closed(self.window) {
            return Err(CaptureError::Closed);
        }
        let Some(duplication) = &self.duplication else {
            return self.reopen(device);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-parameters are written on success; a zero timeout never blocks.
        match unsafe { duplication.AcquireNextFrame(0, &raw mut info, &raw mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Frame::Unchanged),
            Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                self.duplication = None;
                return self.reopen(device);
            }
            Err(error) => return Err(failed("take the desktop image")(error)),
        }

        // A frame that only moved the cursor keeps the previous picture.
        let frame = if info.LastPresentTime != 0 || !self.primed {
            resource
                .ok_or_else(windows::core::Error::empty)
                .and_then(|resource| resource.cast::<ID3D11Texture2D>())
                .map(|texture| {
                    device.copy_region(target, &texture, width, height);
                    self.primed = true;
                    Frame::Fresh
                })
                .map_err(failed("read the desktop image"))
        } else {
            Ok(Frame::Unchanged)
        };
        // Hands the surface back to Windows; the copy above is already queued on the GPU.
        // SAFETY: releases the frame acquired above.
        let _ = unsafe { duplication.ReleaseFrame() };
        frame
    }
}

/// The output of the device's adapter that shows `monitor`.
fn find_output(device: &Device, monitor: HMONITOR) -> Result<IDXGIOutput1, CaptureError> {
    let adapter = device
        .dxgi()
        // SAFETY: reads the adapter of a live device.
        .and_then(|dxgi| unsafe { dxgi.GetAdapter() })
        .map_err(failed("find the GPU's monitors"))?;
    for index in 0.. {
        // SAFETY: enumeration ends with DXGI_ERROR_NOT_FOUND.
        let output = match unsafe { adapter.EnumOutputs(index) } {
            Ok(output) => output,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(failed("list the GPU's monitors")(error)),
        };
        // SAFETY: reads the description of a live output.
        let description = unsafe { output.GetDesc() }.map_err(failed("describe a monitor"))?;
        if description.Monitor == monitor {
            return output
                .cast()
                .map_err(failed("open the monitor for duplication"));
        }
    }
    Err(CaptureError::Unsupported(format!(
        "Desktop Duplication can only capture monitors connected to {}, the GPU that encodes; choose Windows Graphics Capture",
        device.adapter
    )))
}

/// Opens a duplication of `output` for the device, with its description.
fn duplicate(
    output: &IDXGIOutput1,
    device: &Device,
) -> windows::core::Result<(IDXGIOutputDuplication, DXGI_OUTDUPL_DESC)> {
    // SAFETY: duplicates a live output for a live device on the output's adapter.
    let duplication = unsafe { output.DuplicateOutput(&device.d3d) }?;
    // SAFETY: describes a live duplication.
    let description = unsafe { duplication.GetDesc() };
    Ok((duplication, description))
}

/// The error for a monitor that could not be duplicated when the session started; a monitor
/// Windows will never duplicate reads as unsupported.
fn first_duplication_failed(error: windows::core::Error) -> CaptureError {
    if error.code() == DXGI_ERROR_UNSUPPORTED {
        return CaptureError::Unsupported(
            "Desktop Duplication is not available for this monitor; choose Windows Graphics Capture"
                .to_owned(),
        );
    }
    failed("duplicate the monitor")(error)
}

/// The duplicated monitor's size in pixels.
fn size(description: &DXGI_OUTDUPL_DESC) -> (u32, u32) {
    (description.ModeDesc.Width, description.ModeDesc.Height)
}

/// Whether the desktop image arrives turned: duplication delivers a rotated monitor's image
/// in the panel's own orientation.
fn is_rotated(description: &DXGI_OUTDUPL_DESC) -> bool {
    ![DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED].contains(&description.Rotation)
}
