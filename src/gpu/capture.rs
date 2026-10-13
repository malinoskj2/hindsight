//! Capture of a monitor or a window into the encoder's input, by one of several backends.

mod display;
#[expect(unsafe_code)]
mod dxgi_dd;
#[expect(unsafe_code)]
mod nvfbc;
mod tonemap;
#[expect(unsafe_code)]
mod wgc;

use crate::{
    config::Backend,
    gpu::{
        cuda::{Context, CudaError, DeviceBuffer},
        device::Device,
    },
    log,
    replay::Matrix,
    service::Target,
};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::{
        Direct3D9::{IDirect3DDevice9Ex, IDirect3DSurface9},
        Direct3D11::ID3D11Texture2D,
        Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint},
    },
    UI::WindowsAndMessaging::IsWindow,
};

const REOPEN_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub(super) enum CaptureError {
    #[error("{0}")]
    Unsupported(String),
    #[error("the captured display or window went away")]
    Closed,
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
    #[error("{0} failed with {1}")]
    Nvfbc(&'static str, nvfbc::Status),
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    #[error("{0}")]
    Restart(&'static str),
}

/// A source-owned surface the encoder reads in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Surface {
    Texture(ID3D11Texture2D),
    /// BGRA rows `pitch` bytes apart in the source's CUDA context.
    CudaBuffer {
        buffer: Arc<DeviceBuffer>,
        pitch: u32,
    },
    /// BGRA on the source's Direct3D 9Ex device.
    Direct3D9(IDirect3DSurface9),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EncoderDevice {
    Direct3D,
    /// `primary` is the device whose primary context this is, so memory that must outlive
    /// the source can retain it again.
    Cuda {
        context: Context,
        primary: Option<super::cuda::Device>,
    },
    /// Capture thread only.
    Direct3D9(IDirect3DDevice9Ex),
}

pub(super) enum Frame {
    Fresh,
    /// Left in the source's own surface with this index; only after
    /// [`Source::hand_over_surfaces`].
    InSurface(usize),
    Unchanged,
    Resized,
}

/// Where a source that copies frames puts them: the top-left `width` × `height` of `texture`.
#[derive(Clone, Copy)]
pub(super) struct CopyTarget<'a> {
    pub(super) device: &'a Device,
    pub(super) texture: &'a ID3D11Texture2D,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) trait Source {
    fn size(&self) -> (u32, u32);

    /// A window capture may never deliver a frame: exclusive full screen is not composed.
    fn captures_window(&self) -> bool;

    /// Copies the newest frame into `target`; there is none when the encoder runs on CUDA or
    /// reads the source's surfaces in place.
    fn take(&mut self, target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError>;

    fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Direct3D
    }

    /// For YUV surfaces; BGRA is converted by NVENC with BT.709.
    fn colour_matrix(&self) -> Matrix {
        Matrix::Bt709
    }

    /// Switches to leaving each picture in a surface of its own, reported as
    /// [`Frame::InSurface`]; reports whether it did.
    fn hand_over_surfaces(&mut self) -> bool {
        false
    }

    /// Stays the same memory for the session.
    fn surface(&self, _index: usize) -> Option<Surface> {
        None
    }

    /// The surface the next [`take`](Self::take) captures into; the encoder must be done with it.
    fn next_surface(&self) -> Option<usize> {
        None
    }

    /// Hands back the surfaces for which `in_use` is false.
    fn release_surfaces(&mut self, _in_use: &dyn Fn(usize) -> bool) {}
}

#[derive(Default)]
struct ReopenThrottle {
    due: Option<Instant>,
}

impl ReopenThrottle {
    /// `Some(now)` when an attempt is due.
    fn attempt(&self) -> Option<Instant> {
        let now = Instant::now();
        self.due.is_none_or(|due| now >= due).then_some(now)
    }

    fn succeeded(&mut self) {
        self.due = None;
    }

    fn refused(&mut self, now: Instant) {
        self.due = Some(now + REOPEN_INTERVAL);
    }
}

pub(super) fn start(
    backend: Backend,
    device: &Device,
    period: Duration,
    target: &Target,
    cursor: bool,
) -> Result<Box<dyn Source>, CaptureError> {
    let display = display::describe(monitor_of(target).0);
    if let Some(display) = &display {
        log::info(format!("Display {display}"));
    }
    let hdr = display.as_ref().is_some_and(display::Display::is_hdr);
    let tone_map = match (colour(backend, hdr), &display) {
        (Colour::ToneMapped, Some(display)) => tone_mapper(device, display),
        (Colour::Clipped, _) => {
            warn_hdr_clipped();
            None
        }
        _ => None,
    };
    Ok(match backend {
        Backend::Wgc => Box::new(wgc::Capture::start(
            device, period, target, cursor, tone_map,
        )?),
        Backend::Dxgi => Box::new(dxgi_dd::Capture::start(device, target, tone_map)?),
        Backend::Nvfbc => Box::new(nvfbc::Capture::start(device, target, cursor)?),
        Backend::NvfbcCuda => Box::new(nvfbc::CudaCapture::start(device, target, cursor)?),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Colour {
    Sdr,
    ToneMapped,
    /// 8-bit from a display in HDR: the backend cannot deliver more.
    Clipped,
}

const fn colour(backend: Backend, hdr: bool) -> Colour {
    match (backend, hdr) {
        (_, false) => Colour::Sdr,
        (Backend::Wgc | Backend::Dxgi, true) => Colour::ToneMapped,
        (Backend::Nvfbc | Backend::NvfbcCuda, true) => Colour::Clipped,
    }
}

fn tone_mapper(device: &Device, display: &display::Display) -> Option<tonemap::ToneMapper> {
    let curve = tonemap::Curve::new(display.sdr_white_nits, display.max_nits);
    match tonemap::ToneMapper::new(device, curve) {
        Ok(mapper) => {
            log::info(format!(
                "The display is in HDR: capturing scRGB and tone-mapping it to SDR, white at {:.0} nits, highlights rolled off up to {:.0} nits",
                curve.white_nits(),
                curve.peak_nits()
            ));
            Some(mapper)
        }
        Err(error) => {
            log::warning(format!(
                "The display is in HDR but the tone-map pass could not be prepared ({error}); highlights are clipped"
            ));
            None
        }
    }
}

fn warn_hdr_clipped() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log::warning(
            "The display is in HDR, which NvFBC captures as 8-bit SDR: highlights are clipped and colours may look washed out. Windows Graphics Capture records HDR displays properly",
        );
    }
}

fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |error| CaptureError::Windows(step, error)
}

fn target_of(target: Option<CopyTarget<'_>>) -> CopyTarget<'_> {
    target.expect("a source that copies frames runs with an encoder on the Direct3D device")
}

/// Puts `source` into `target`, through the tone-map pass when there is one.
fn deliver(
    tone_map: Option<&mut tonemap::ToneMapper>,
    source: &ID3D11Texture2D,
    target: CopyTarget<'_>,
) -> windows::core::Result<()> {
    let CopyTarget {
        device,
        texture,
        width,
        height,
    } = target;
    if let Some(mapper) = tone_map {
        return mapper.convert(device, source, texture, width, height);
    }
    device.copy_region(texture, source, width, height);
    Ok(())
}

/// The monitor to record, and the window it stands in for when the target is one.
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

#[expect(unsafe_code)]
fn monitor(handle: isize) -> HMONITOR {
    if handle == 0 {
        // SAFETY: (0, 0) always lies on the primary monitor.
        unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
    } else {
        HMONITOR(handle as *mut c_void)
    }
}

#[expect(unsafe_code)]
fn is_window_closed(window: Option<HWND>) -> bool {
    // SAFETY: IsWindow accepts any handle, live or not.
    window.is_some_and(|window| !unsafe { IsWindow(Some(window)) }.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_hdr_display_changes_how_colour_is_captured() {
        for backend in Backend::ALL {
            assert_eq!(colour(backend, false), Colour::Sdr);
        }
        assert_eq!(colour(Backend::Wgc, true), Colour::ToneMapped);
        assert_eq!(colour(Backend::Dxgi, true), Colour::ToneMapped);
        assert_eq!(colour(Backend::Nvfbc, true), Colour::Clipped);
        assert_eq!(colour(Backend::NvfbcCuda, true), Colour::Clipped);
    }

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
