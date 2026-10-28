//! Capture of a monitor or a window into the encoder's input, by one of several backends.

mod display;
#[expect(unsafe_code, reason = "calls DXGI Desktop Duplication and Direct3D 11")]
mod dxgi_dd;
#[expect(unsafe_code, reason = "binds the NvFBC driver API")]
mod nvfbc;
mod tonemap;
#[expect(
    unsafe_code,
    reason = "calls Windows Graphics Capture and its Direct3D interop"
)]
mod wgc;

use crate::{
    config::Backend,
    gpu::{
        cuda::{ContextHold, CudaError, DeviceBuffer},
        cuda_output::CudaRing,
        device::Device,
    },
    log,
    replay::Matrix,
    service::{MonitorHandle, Target},
    sys::{
        foreground::AppWindow,
        view::{Decision, View},
    },
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
use windows::Win32::Graphics::{
    Direct3D9::{IDirect3DDevice9Ex, IDirect3DSurface9},
    Direct3D11::ID3D11Texture2D,
    Gdi::HMONITOR,
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

pub(super) enum EncoderDevice {
    Direct3D,
    /// The source's CUDA context, held for as long as the encoder keeps this value. A primary
    /// context is kept alive by the hold; a context the NvFBC driver opened is kept alive by the
    /// source's session. When `ring` is given, the encoded frames are written into it in video
    /// memory, which only a CUDA session can do.
    Cuda {
        hold: Arc<ContextHold>,
        ring: Option<CudaRing>,
    },
    /// Capture thread only.
    Direct3D9(IDirect3DDevice9Ex),
}

pub(super) enum Frame {
    Fresh,
    /// The frame was left in the source's own surface, number `index` among them. Reported
    /// only after [`Capture::hand_over_surfaces`].
    InSurface {
        index: usize,
        surface: Surface,
    },
    Unchanged,
    Resized,
}

/// The destination of a source that copies frames: the top-left `width` × `height` region of
/// `texture`.
#[derive(Clone, Copy)]
pub(super) struct CopyTarget<'a> {
    pub(super) device: &'a Device,
    pub(super) texture: &'a ID3D11Texture2D,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// The operations every backend supports. The rest differ by backend and are on [`Capture`].
trait Source {
    fn size(&self) -> (u32, u32);

    /// Whether this captures a window. A window capture may never deliver a frame, because an
    /// exclusive full-screen window is not composed.
    fn captures_window(&self) -> bool;

    /// Copies the newest frame into `target`. There is no target when the encoder runs on CUDA
    /// or reads the source's surfaces in place.
    fn take(&mut self, target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError>;

    /// Called instead of [`take`](Self::take) while the monitor shows something other than the
    /// app recorded through it. Drops any frame kept for the next `take`, so that a picture of
    /// what covered the app is not delivered once it is back.
    fn hold(&mut self) {}
}

pub(super) struct Capture {
    backend: ByBackend,
    app: Option<Watched>,
}

enum ByBackend {
    Wgc(wgc::Capture),
    Dxgi(dxgi_dd::Capture),
    Nvfbc(nvfbc::Capture),
    NvfbcCuda(nvfbc::CudaCapture),
}

impl Capture {
    fn source(&self) -> &dyn Source {
        match &self.backend {
            ByBackend::Wgc(capture) => capture,
            ByBackend::Dxgi(capture) => capture,
            ByBackend::Nvfbc(capture) => capture,
            ByBackend::NvfbcCuda(capture) => capture,
        }
    }

    fn source_mut(&mut self) -> &mut dyn Source {
        match &mut self.backend {
            ByBackend::Wgc(capture) => capture,
            ByBackend::Dxgi(capture) => capture,
            ByBackend::Nvfbc(capture) => capture,
            ByBackend::NvfbcCuda(capture) => capture,
        }
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.source().size()
    }

    /// Whether this captures a window. A window capture may never deliver a frame, because an
    /// exclusive full-screen window is not composed.
    pub(super) fn captures_window(&self) -> bool {
        self.source().captures_window()
    }

    /// Copies the newest frame into `target`. There is no target when the encoder runs on CUDA
    /// or reads the source's surfaces in place.
    pub(super) fn take(&mut self, target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError> {
        if let Some(app) = &mut self.app {
            match app.decide() {
                Decision::Record => {}
                Decision::Hold => {
                    self.source_mut().hold();
                    return Ok(Frame::Unchanged);
                }
                Decision::Closed => return Err(CaptureError::Closed),
            }
        }
        self.source_mut().take(target)
    }

    /// The device the encoder must run on to read this capture's surfaces.
    pub(super) fn encoder_device(&self) -> EncoderDevice {
        match &self.backend {
            ByBackend::Wgc(_) | ByBackend::Dxgi(_) => EncoderDevice::Direct3D,
            ByBackend::Nvfbc(capture) => capture.encoder_device(),
            ByBackend::NvfbcCuda(capture) => capture.encoder_device(),
        }
    }

    /// The colour matrix for YUV surfaces. Every backend delivers BGRA, which NVENC converts
    /// with BT.709.
    #[expect(
        clippy::unused_self,
        reason = "a backend delivering YUV would answer for itself"
    )]
    pub(super) const fn colour_matrix(&self) -> Matrix {
        Matrix::Bt709
    }

    /// Switches to leaving each picture in a surface of its own, reported as
    /// [`Frame::InSurface`]. Returns whether the switch was made.
    pub(super) fn hand_over_surfaces(&mut self) -> bool {
        match &mut self.backend {
            ByBackend::Wgc(capture) => capture.hand_over_surfaces(),
            ByBackend::Dxgi(_) => false,
            ByBackend::Nvfbc(capture) => capture.hand_over_surfaces(),
            ByBackend::NvfbcCuda(capture) => capture.hand_over_surfaces(),
        }
    }

    /// The surface the next [`take`](Self::take) captures into. The encoder must be done with
    /// it first.
    pub(super) fn next_surface(&self) -> Option<usize> {
        match &self.backend {
            ByBackend::Wgc(_) | ByBackend::Dxgi(_) => None,
            ByBackend::Nvfbc(capture) => capture.next_surface(),
            ByBackend::NvfbcCuda(capture) => capture.next_surface(),
        }
    }

    /// Releases the surfaces for which `in_use` is false.
    pub(super) fn release_surfaces(&mut self, in_use: &dyn Fn(usize) -> bool) {
        match &mut self.backend {
            ByBackend::Wgc(capture) => capture.release_surfaces(in_use),
            ByBackend::Dxgi(_) | ByBackend::Nvfbc(_) | ByBackend::NvfbcCuda(_) => {}
        }
    }
}

struct Watched {
    window: AppWindow,
    view: View,
    name: String,
}

impl Watched {
    fn of(target: &Target, captures_window: bool) -> Option<Self> {
        match target {
            Target::Window {
                window,
                monitor,
                name,
                ..
            } if !captures_window => Some(Self {
                window: AppWindow::new(*window, *monitor),
                view: View::default(),
                name: name.clone(),
            }),
            _ => None,
        }
    }

    fn decide(&mut self) -> Decision {
        let was_shown = self.view == View::Shown;
        let decision = self.view.update(self.window.sight(), Instant::now());
        match (was_shown, self.view == View::Shown) {
            (true, false) => log::info(format!(
                "Capture · {} out of view · its last picture held",
                self.name
            )),
            (false, true) => log::info(format!("Capture · {} back in view", self.name)),
            _ => {}
        }
        decision
    }
}

#[derive(Default)]
struct ReopenThrottle {
    due: Option<Instant>,
}

impl ReopenThrottle {
    /// Returns `Some(now)` when an attempt is due.
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
) -> Result<Capture, CaptureError> {
    let display = display::describe(monitor_of(target));
    if let Some(display) = &display {
        log::info(format!("Display · {display}"));
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
    let backend = match backend {
        Backend::Wgc => ByBackend::Wgc(wgc::Capture::start(
            device, period, target, cursor, tone_map,
        )?),
        Backend::Dxgi => ByBackend::Dxgi(dxgi_dd::Capture::start(device, target, tone_map)?),
        Backend::Nvfbc => ByBackend::Nvfbc(nvfbc::Capture::start(device, target, cursor)?),
        Backend::NvfbcCuda => {
            ByBackend::NvfbcCuda(nvfbc::CudaCapture::start(device, target, cursor)?)
        }
    };
    let mut capture = Capture { backend, app: None };
    capture.app = Watched::of(target, capture.captures_window());
    Ok(capture)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Colour {
    Sdr,
    ToneMapped,
    /// 8-bit colour from a display in HDR, because the backend cannot deliver more.
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
                "HDR · tone-mapped to SDR · white {:.0} nits · roll-off to {:.0} nits",
                curve.white_nits(),
                curve.peak_nits()
            ));
            Some(mapper)
        }
        Err(error) => {
            log::warning(format!(
                "HDR · tone-map unavailable ({error}) · highlights clipped"
            ));
            None
        }
    }
}

fn warn_hdr_clipped() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log::warning("HDR · NvFBC clips it to 8-bit SDR · Windows Graphics Capture tone-maps");
    }
}

fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |error| CaptureError::Windows(step, error)
}

/// A copying source reports [`EncoderDevice::Direct3D`], whose encoder always has an input to
/// copy into. Without one there is nothing to deliver to, and the session is restarted.
fn target_of(target: Option<CopyTarget<'_>>) -> Result<CopyTarget<'_>, CaptureError> {
    target.ok_or(CaptureError::Restart(
        "the encoder has no Direct3D input to copy the captured frame into",
    ))
}

/// Copies `source` into `target`, through the tone-map pass when there is one.
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

fn monitor_of(target: &Target) -> HMONITOR {
    match *target {
        Target::Monitor { handle, .. } => monitor(handle),
        Target::Window {
            monitor: handle, ..
        } => monitor(Some(handle)),
    }
}

fn monitor(handle: Option<MonitorHandle>) -> HMONITOR {
    match handle {
        Some(handle) => HMONITOR(handle.get() as *mut c_void),
        // (0, 0) always lies on the primary monitor.
        None => HMONITOR(
            winsafe::HMONITOR::MonitorFromPoint(
                winsafe::POINT::default(),
                winsafe::co::MONITOR::DEFAULTTOPRIMARY,
            )
            .ptr(),
        ),
    }
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
