//! DXGI Desktop Duplication of a whole monitor on the encoding GPU, cursor not drawn; a
//! window target is recorded through the monitor it fills.

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

/// Retried: secure desktop up, too many duplications, session switch, desktop still changing.
const PASSING_REFUSALS: [HRESULT; 4] = [
    E_ACCESSDENIED,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_ACCESS_LOST,
];

/// Preferred first.
const SCRGB_FORMATS: [DXGI_FORMAT; 2] =
    [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];

pub(super) struct Capture {
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    size: (u32, u32),
    window: Option<HWND>,
    /// A fresh duplication's first frame is always new.
    primed: bool,
    reopen: ReopenThrottle,
    tone_map: Option<ToneMapper>,
    format: DXGI_FORMAT,
}

enum Acquired {
    /// Held until released.
    Frame(DXGI_OUTDUPL_FRAME_INFO, Option<IDXGIResource>),
    Nothing,
    Lost,
}

impl Capture {
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
        // HDR toggled: restart the session.
        if description.ModeDesc.Format != self.format {
            log::info("The captured display's colour format changed");
            return Ok(Frame::Resized);
        }
        self.duplication = Some(duplication);
        self.primed = false;
        Ok(Frame::Unchanged)
    }

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
        // SAFETY: releases the frame acquired above; the copy is already queued.
        let _ = unsafe { duplication.ReleaseFrame() };
        frame
    }
}

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

/// `DuplicateOutput1` refuses a thread that is not per-monitor DPI aware.
fn duplicate_scrgb(
    output: &IDXGIOutput1,
    device: &Device,
) -> windows::core::Result<IDXGIOutputDuplication> {
    let output: IDXGIOutput5 = output.cast()?;
    // SAFETY: changes the calling thread's DPI awareness only, and is undone below.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: live output and device on its adapter; the format list outlives the call.
    let duplication = unsafe { output.DuplicateOutput1(&device.d3d, 0, &SCRGB_FORMATS) };
    if !previous.is_invalid() {
        // SAFETY: puts back the thread's awareness from before.
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    duplication
}

fn first_duplication_failed(error: windows::core::Error) -> CaptureError {
    if error.code() == DXGI_ERROR_UNSUPPORTED {
        return CaptureError::Unsupported(
            "Desktop Duplication is not available for this monitor; choose Windows Graphics Capture"
                .to_owned(),
        );
    }
    failed("duplicate the monitor")(error)
}

fn size(description: &DXGI_OUTDUPL_DESC) -> (u32, u32) {
    (description.ModeDesc.Width, description.ModeDesc.Height)
}

/// Duplication delivers a rotated monitor in the panel's own orientation.
fn is_rotated(description: &DXGI_OUTDUPL_DESC) -> bool {
    ![DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED].contains(&description.Rotation)
}

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
