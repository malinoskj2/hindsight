//! Windows Graphics Capture of a monitor or a window. Once handed over, NVENC reads the pool's
//! buffers in place. A display in HDR is tone-mapped from FP16 instead.

use super::{
    CaptureError, CopyTarget, Frame, Source, Surface, deliver, display, failed, monitor,
    monitor_of, tonemap::ToneMapper,
};
use crate::{
    gpu::{device::Device, pacer::filetime_units},
    log,
    service::Target,
    sys::com::Apartment,
};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::HWND,
        Graphics::{Direct3D11::ID3D11Texture2D, Gdi::HMONITOR},
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    core::{Interface, factory},
};

const POOL_BUFFERS: i32 = 2;
/// As many as the encoder has frames in flight.
const HELD_BUFFERS: i32 = 3;
/// A pool that hands out more surfaces has replaced its buffers, which are then copied instead.
const MAX_SURFACES: usize = 8;
const HDR_CHECK_INTERVAL: Duration = Duration::from_secs(2);

pub(super) struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    captures_window: bool,
    closed: Arc<AtomicBool>,
    closed_token: i64,
    direct3d: IDirect3DDevice,
    handed_over: bool,
    surfaces: Vec<ID3D11Texture2D>,
    /// Holds each surface's frame while the encoder still needs it.
    held: Vec<Option<Direct3D11CaptureFrame>>,
    tone_map: Option<ToneMapper>,
    monitor: HMONITOR,
    next_hdr_check: Instant,
    /// The apartment the capture objects live in. Declared last so that they are all dropped
    /// first.
    _apartment: Apartment,
}

impl Capture {
    /// Starts a capture. Frames are limited to twice per `period`, because a limit of one period
    /// skips every other refresh when the rates do not divide evenly.
    pub(super) fn start(
        device: &Device,
        period: Duration,
        target: &Target,
        cursor: bool,
        tone_map: Option<ToneMapper>,
    ) -> Result<Self, CaptureError> {
        let apartment = prepare_thread()?;
        let item = capture_item(target)?;
        let size = item.Size().map_err(failed("read the capture size"))?;
        let (closed, closed_token) = watch_closed(&item)?;
        let direct3d = winrt_device(device)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d,
            pixel_format(tone_map.as_ref()),
            POOL_BUFFERS,
            size,
        )
        .map_err(failed("create the capture frame pool"))?;
        let session = start_session(&pool, &item, period, cursor)?;

        Ok(Self {
            item,
            pool,
            session,
            size,
            captures_window: matches!(target, Target::Window { .. }),
            closed,
            closed_token,
            direct3d,
            handed_over: false,
            surfaces: Vec::new(),
            held: Vec::new(),
            tone_map,
            monitor: monitor_of(target).0,
            next_hdr_check: Instant::now() + HDR_CHECK_INTERVAL,
            _apartment: apartment,
        })
    }
}

impl Source for Capture {
    fn size(&self) -> (u32, u32) {
        let pixels = |length| u32::try_from(length).expect("a capture size is never negative");
        (pixels(self.size.Width), pixels(self.size.Height))
    }

    fn captures_window(&self) -> bool {
        self.captures_window
    }

    fn take(&mut self, target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(CaptureError::Closed);
        }
        let target = super::target_of(target)?;
        if self.hdr_turned_off() {
            log::info("Display · HDR turned off");
            return Ok(Frame::Resized);
        }

        let Some(frame) = newest_frame(&self.pool) else {
            return Ok(Frame::Unchanged);
        };

        let content = frame.ContentSize().map_err(failed("read the frame size"))?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            let _ = frame.Close();
            return Ok(Frame::Resized);
        }

        let texture = frame_texture(&frame)?;
        if self.handed_over
            && let Some(index) = self.surface_index(&texture)
        {
            if let Some(older) = self.held[index].replace(frame) {
                let _ = older.Close();
            }
            return Ok(Frame::InSurface {
                index,
                surface: Surface::Texture(texture),
            });
        }
        // Either not handed over (always the case in HDR), or the pool replaced its buffers.
        let copied = deliver(self.tone_map.as_mut(), &texture, target);
        let _ = frame.Close();
        copied
            .map(|()| Frame::Fresh)
            .map_err(failed("tone-map the HDR frame"))
    }
}

impl Capture {
    pub(super) fn hand_over_surfaces(&mut self) -> bool {
        // NVENC cannot read FP16 frames.
        if self.tone_map.is_some() {
            return false;
        }
        // Nothing has been taken from the pool yet.
        let recreated = self.pool.Recreate(
            &self.direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS + HELD_BUFFERS,
            self.size,
        );
        self.handed_over = recreated.is_ok();
        self.handed_over
    }

    pub(super) fn release_surfaces(&mut self, in_use: &dyn Fn(usize) -> bool) {
        for (index, held) in self.held.iter_mut().enumerate() {
            if !in_use(index)
                && let Some(frame) = held.take()
            {
                let _ = frame.Close();
            }
        }
    }
}

impl Capture {
    /// Returns whether the display has left HDR. An SDR capture never checks, so a display
    /// entering HDR is recorded clipped.
    fn hdr_turned_off(&mut self) -> bool {
        if self.tone_map.is_none() {
            return false;
        }
        let now = Instant::now();
        if now < self.next_hdr_check {
            return false;
        }
        self.next_hdr_check = now + HDR_CHECK_INTERVAL;
        display::is_in_hdr(self.monitor) == Some(false)
    }

    fn surface_index(&mut self, texture: &ID3D11Texture2D) -> Option<usize> {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|known| known.as_raw() == texture.as_raw())
        {
            return Some(index);
        }
        if self.surfaces.len() >= MAX_SURFACES {
            return None;
        }
        self.surfaces.push(texture.clone());
        self.held.push(None);
        Some(self.surfaces.len() - 1)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
        for frame in self.held.iter_mut().filter_map(Option::take) {
            let _ = frame.Close();
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Enters the multithreaded apartment, which is what `RoInitialize(RO_INIT_MULTITHREADED)` sets
/// up, until the returned guard is dropped on this thread.
fn prepare_thread() -> Result<Apartment, CaptureError> {
    let apartment =
        Apartment::multi_threaded().map_err(failed("start WinRT on the capture thread"))?;
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(CaptureError::Unsupported(
            "Windows Graphics Capture is not available; Windows 10 version 1903 or newer is needed"
                .to_owned(),
        ));
    }
    Ok(apartment)
}

fn capture_item(target: &Target) -> Result<GraphicsCaptureItem, CaptureError> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(failed("open the capture interop"))?;
    match *target {
        Target::Monitor { handle, .. } => {
            // SAFETY: a stale handle makes the call fail, not misbehave.
            unsafe { interop.CreateForMonitor(monitor(handle)) }
                .map_err(failed("capture the monitor"))
        }
        Target::Window { window, .. } => {
            // SAFETY: as above, for a window handle.
            unsafe { interop.CreateForWindow(HWND(window.get() as *mut c_void)) }
                .map_err(failed("capture the window"))
        }
    }
}

fn watch_closed(item: &GraphicsCaptureItem) -> Result<(Arc<AtomicBool>, i64), CaptureError> {
    let closed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&closed);
    let token = item
        .Closed(&TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        }))
        .map_err(failed("watch the capture item"))?;
    Ok((closed, token))
}

fn winrt_device(device: &Device) -> Result<IDirect3DDevice, CaptureError> {
    let dxgi = device
        .dxgi()
        .map_err(failed("share the device with capture"))?;
    // SAFETY: wraps the live device for the WinRT capture API.
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .and_then(|inspectable| inspectable.cast())
        .map_err(failed("share the device with capture"))
}

fn start_session(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    period: Duration,
    cursor: bool,
) -> Result<GraphicsCaptureSession, CaptureError> {
    let session = pool
        .CreateCaptureSession(item)
        .map_err(failed("create the capture session"))?;
    // Each setting needs a newer Windows than capture itself. Without it, the default stays.
    let _ = session.SetIsCursorCaptureEnabled(cursor);
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetMinUpdateInterval(TimeSpan {
        Duration: filetime_units(period / 2),
    });
    session.StartCapture().map_err(failed("start capturing"))?;
    Ok(session)
}

/// The pool reports having no frame as an error.
fn newest_frame(pool: &Direct3D11CaptureFramePool) -> Option<Direct3D11CaptureFrame> {
    let mut newest = None;
    while let Ok(frame) = pool.TryGetNextFrame() {
        if let Some(older) = newest.replace(frame) {
            let _ = older.Close();
        }
    }
    newest
}

fn frame_texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D, CaptureError> {
    frame
        .Surface()
        .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
        // SAFETY: the surface of a live frame wraps a Direct3D 11 texture.
        .and_then(|access| unsafe { access.GetInterface() })
        .map_err(failed("read the captured frame"))
}

fn pixel_format(tone_map: Option<&ToneMapper>) -> DirectXPixelFormat {
    if tone_map.is_some() {
        DirectXPixelFormat::R16G16B16A16Float
    } else {
        DirectXPixelFormat::B8G8R8A8UIntNormalized
    }
}
