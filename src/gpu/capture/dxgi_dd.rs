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
//!
//! A monitor in HDR is duplicated as FP16 scRGB, which a tone-map pass turns into the 8-bit
//! input instead of the copy; Windows would otherwise convert it to 8 bits by clipping. HDR
//! turned on or off changes the duplication's format, which restarts the session.

use super::{
    CaptureError, Frame, ReopenThrottle, Source, failed, is_window_closed, monitor_of,
    tonemap::ToneMapper,
};
use crate::{gpu::device::Device, log, service::Target};
use std::time::Instant;
use windows::{
    Win32::{
        Foundation::{E_ACCESSDENIED, HWND},
        Graphics::{
            Direct3D11::ID3D11Texture2D,
            Dxgi::{
                Common::{
                    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
                    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
                },
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND,
                DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutput5,
                IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::HMONITOR,
        },
        UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
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

/// The formats a duplication of a monitor in HDR may deliver, preferred first: the FP16 scRGB
/// desktop, or 8-bit BGRA when the desktop is not composed in FP16.
const SCRGB_FORMATS: [DXGI_FORMAT; 2] =
    [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];

/// A running Desktop Duplication of one monitor.
pub(super) struct Capture {
    /// The duplicated monitor, as an output of the encoding GPU.
    output: IDXGIOutput1,
    /// `None` while Windows has taken it away.
    duplication: Option<IDXGIOutputDuplication>,
    size: (u32, u32),
    /// The window whose monitor is recorded, when the target is one.
    window: Option<HWND>,
    /// Whether the current duplication has delivered a picture; its first one is always new.
    primed: bool,
    reopen: ReopenThrottle,
    /// The pass that turns the scRGB desktop of a monitor in HDR into the encoder's 8-bit
    /// input; `None` for a monitor in SDR, which is duplicated as 8-bit BGRA.
    tone_map: Option<ToneMapper>,
    /// The format the duplication delivers: FP16 when a tone-mapped monitor is in HDR, BGRA
    /// otherwise.
    format: DXGI_FORMAT,
}

/// What asking Windows for the next desktop image came to.
enum Acquired {
    /// A frame is held until it is released: its description, and its surface when it has one.
    Frame(DXGI_OUTDUPL_FRAME_INFO, Option<IDXGIResource>),
    /// Nothing changed since the last frame.
    Nothing,
    /// Windows took the duplication away.
    Lost,
}

impl Capture {
    /// Starts duplicating the monitor `target` names, or the monitor its window fills. With
    /// `tone_map`, given for a monitor in HDR, the desktop is asked for as FP16 scRGB and
    /// passed through it into the encoder's input.
    pub(super) fn start(
        device: &Device,
        target: &Target,
        tone_map: Option<ToneMapper>,
    ) -> Result<Self, CaptureError> {
        let (monitor, window) = monitor_of(target);
        let output = find_output(device, monitor)?;
        let (duplication, description) =
            duplicate(&output, device, tone_map.is_some()).map_err(first_duplication_failed)?;
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
            reopen: ReopenThrottle::default(),
            tone_map,
            format: description.ModeDesc.Format,
        })
    }

    /// Opens the duplication again after Windows took it away. While Windows refuses, the
    /// last picture is repeated; a new size or orientation ends the session.
    fn reopen(&mut self, device: &Device) -> Result<Frame, CaptureError> {
        let Some(now) = self.reopen.attempt() else {
            return Ok(Frame::Unchanged);
        };
        let (duplication, description) =
            match duplicate(&self.output, device, self.tone_map.is_some()) {
                Ok(opened) => opened,
                Err(error) if PASSING_REFUSALS.contains(&error.code()) => {
                    return self.retry_later(now);
                }
                Err(error) => return Err(failed("duplicate the monitor again")(error)),
            };
        self.reopen.succeeded();
        if size(&description) != self.size || is_rotated(&description) {
            return Ok(Frame::Resized);
        }
        // HDR turned on or off: the session starts again, deciding afresh how to capture.
        if description.ModeDesc.Format != self.format {
            log::info("The captured display's colour format changed");
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
        self.reopen.refused(now);
        Ok(Frame::Unchanged)
    }
}

impl Source for Capture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn captures_window(&self) -> bool {
        false
    }

    fn take(
        &mut self,
        device: &Device,
        target: Option<&ID3D11Texture2D>,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        if is_window_closed(self.window) {
            return Err(CaptureError::Closed);
        }
        let target = super::target_of(target);

        let Some(duplication) = &self.duplication else {
            return self.reopen(device);
        };

        let (info, resource) = match acquire(duplication)? {
            Acquired::Frame(info, resource) => (info, resource),
            Acquired::Nothing => return Ok(Frame::Unchanged),
            Acquired::Lost => {
                self.duplication = None;
                return self.reopen(device);
            }
        };

        // A frame that only moved the cursor keeps the previous picture.
        let frame = if info.LastPresentTime != 0 || !self.primed {
            let scrgb = self.format == DXGI_FORMAT_R16G16B16A16_FLOAT;
            let delivered = resource
                .ok_or_else(windows::core::Error::empty)
                .and_then(|resource| resource.cast::<ID3D11Texture2D>())
                .and_then(|texture| match &mut self.tone_map {
                    Some(mapper) if scrgb => {
                        mapper.convert(device, &texture, target, width, height)
                    }
                    _ => {
                        device.copy_region(target, &texture, width, height);
                        Ok(())
                    }
                });
            self.primed |= delivered.is_ok();
            delivered
                .map(|()| Frame::Fresh)
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
/// With `scrgb` the duplication is asked for FP16 scRGB, which it delivers while the monitor
/// is in HDR; where Windows cannot do that, the desktop comes as 8-bit BGRA as it always did.
fn duplicate(
    output: &IDXGIOutput1,
    device: &Device,
    scrgb: bool,
) -> windows::core::Result<(IDXGIOutputDuplication, DXGI_OUTDUPL_DESC)> {
    let duplication = match scrgb.then(|| duplicate_scrgb(output, device)) {
        Some(Ok(duplication)) => duplication,
        Some(Err(error)) if PASSING_REFUSALS.contains(&error.code()) => return Err(error),
        fallback => {
            if let Some(Err(error)) = fallback {
                log::warning(format!(
                    "Desktop Duplication could not deliver the HDR desktop as scRGB ({error}); highlights are clipped"
                ));
            }
            // SAFETY: duplicates a live output for a live device on the output's adapter.
            unsafe { output.DuplicateOutput(&device.d3d) }?
        }
    };
    // SAFETY: describes a live duplication.
    let description = unsafe { duplication.GetDesc() };
    Ok((duplication, description))
}

/// A duplication of `output` that delivers FP16 scRGB when the desktop is composed in it
/// (HDR on), and BGRA when it is not. `DuplicateOutput1` serves only callers aware of each
/// monitor's DPI, which this thread becomes for the call.
fn duplicate_scrgb(
    output: &IDXGIOutput1,
    device: &Device,
) -> windows::core::Result<IDXGIOutputDuplication> {
    let output: IDXGIOutput5 = output.cast()?;
    // SAFETY: changes the calling thread's DPI awareness only, and is undone below.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: duplicates a live output for a live device on the output's adapter, with a
    // list of formats that outlives the call.
    let duplication = unsafe { output.DuplicateOutput1(&device.d3d, 0, &SCRGB_FORMATS) };
    if !previous.is_invalid() {
        // SAFETY: puts back the thread's awareness from before.
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    duplication
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

/// Takes the next desktop image from `duplication` without waiting; a frame it returns is held
/// until it is released.
fn acquire(duplication: &IDXGIOutputDuplication) -> Result<Acquired, CaptureError> {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    // SAFETY: a zero timeout never blocks; both out-parameters are written on success.
    match unsafe { duplication.AcquireNextFrame(0, &raw mut info, &raw mut resource) } {
        Ok(()) => Ok(Acquired::Frame(info, resource)),
        Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Acquired::Nothing),
        Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => Ok(Acquired::Lost),
        Err(error) => Err(failed("take the desktop image")(error)),
    }
}
