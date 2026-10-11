//! Windows Graphics Capture of a monitor or a window. Frames land in a free-threaded frame pool;
//! on each tick the capture thread takes the newest one, copies it into the encoder's input
//! texture on the GPU and hands the pool's buffer straight back to Windows. Once the surfaces
//! are handed over, the frame is not copied: NVENC reads the pool's buffer in place, and the
//! frame is held, keeping Windows from drawing into that buffer, until the encoder is done with
//! it; the pool then has buffers enough for the frames held besides the two it needs itself.

use super::{CaptureError, Frame, Source, failed, monitor};
use crate::{
    gpu::{device::Device, pacer::HUNDRED_NANOSECONDS},
    service::Target,
};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
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
        Graphics::Direct3D11::ID3D11Texture2D,
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize,
        },
    },
    core::{Interface, factory},
};

/// Buffers in the frame pool: one being written by Windows while the newest waits to be taken.
const POOL_BUFFERS: i32 = 2;
/// Frames the encoder may hold at once when it reads the pool's buffers in place: as many as it
/// has frames in flight, the latest picture among them.
const HELD_BUFFERS: i32 = 3;
/// Distinct pool buffers this capture keeps track of; a pool that hands out more than it was
/// created with has replaced its buffers, which are then copied rather than read in place.
const MAX_SURFACES: usize = 8;

/// A running Windows Graphics Capture of one monitor or window.
pub(super) struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    /// Whether the target is a window rather than a monitor.
    captures_window: bool,
    /// Set when Windows ends the capture: the monitor was unplugged, the window closed or the
    /// device was lost.
    closed: Arc<AtomicBool>,
    /// The registration that sets `closed`, removed when dropped.
    closed_token: i64,
    /// The device the pool was created on, to create it again with more buffers.
    direct3d: IDirect3DDevice,
    /// Whether the encoder reads the pool's buffers in place.
    handed_over: bool,
    /// The pool's buffers seen so far, by the index each is reported under.
    surfaces: Vec<ID3D11Texture2D>,
    /// The frame holding each of `surfaces`, while the encoder still needs it.
    held: Vec<Option<Direct3D11CaptureFrame>>,
}

impl Capture {
    /// Starts capturing `target`, with the cursor, at most twice per `period`: the display
    /// delivers frames only on its own refresh, so a limit of exactly one period would skip to
    /// every other refresh whenever the two rates do not divide evenly.
    pub(super) fn start(
        device: &Device,
        period: Duration,
        target: &Target,
    ) -> Result<Self, CaptureError> {
        prepare_thread()?;
        let item = capture_item(target)?;
        let size = item.Size().map_err(failed("read the capture size"))?;
        let (closed, closed_token) = watch_closed(&item)?;
        let direct3d = winrt_device(device)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            size,
        )
        .map_err(failed("create the capture frame pool"))?;
        let session = start_session(&pool, &item, period)?;

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

    fn take(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(CaptureError::Closed);
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
            // Held until the encoder no longer needs the buffer.
            if let Some(older) = self.held[index].replace(frame) {
                let _ = older.Close();
            }
            return Ok(Frame::InSurface(index));
        }
        // Not handed over, or the pool replaced its buffers more often than any pool should.
        device.copy_region(target, &texture, width, height);
        // Returns the buffer to the pool; the copy above is already queued on the GPU.
        let _ = frame.Close();
        Ok(Frame::Fresh)
    }

    fn hand_over_surfaces(&mut self) -> bool {
        // Nothing has been taken from the pool yet, so no frame is lost.
        let recreated = self.pool.Recreate(
            &self.direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS + HELD_BUFFERS,
            self.size,
        );
        self.handed_over = recreated.is_ok();
        self.handed_over
    }

    fn surface(&self, index: usize) -> Option<ID3D11Texture2D> {
        self.surfaces.get(index).cloned()
    }

    fn release_surfaces(&mut self, in_use: &dyn Fn(usize) -> bool) {
        for (index, held) in self.held.iter_mut().enumerate() {
            if held.is_some()
                && !in_use(index)
                && let Some(frame) = held.take()
            {
                let _ = frame.Close();
            }
        }
    }
}

impl Capture {
    /// The index `texture`, one of the pool's buffers, is reported under, given a new one the
    /// first time it comes; `None` once more buffers than any pool keeps have come.
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
        // Stops watching for the end of the capture before ending it, then frees the pool.
        let _ = self.item.RemoveClosed(self.closed_token);
        for frame in self.held.iter_mut().filter_map(Option::take) {
            let _ = frame.Close();
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Puts the calling thread in the multithreaded apartment, where the free-threaded capture
/// objects live, and checks that this Windows offers Graphics Capture at all.
fn prepare_thread() -> Result<(), CaptureError> {
    // A thread already in the apartment stays.
    // SAFETY: initializes WinRT for the calling thread only.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(CaptureError::Unsupported(
            "Windows Graphics Capture is not available; Windows 10 version 1903 or newer is needed"
                .to_owned(),
        ));
    }
    Ok(())
}

/// The capture item for the monitor or window `target` names.
fn capture_item(target: &Target) -> Result<GraphicsCaptureItem, CaptureError> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(failed("open the capture interop"))?;
    match *target {
        Target::Monitor { handle, .. } => {
            // SAFETY: the handle was read from the window being captured moments ago, or
            // names the primary monitor; a stale one makes the call fail, not misbehave.
            unsafe { interop.CreateForMonitor(monitor(handle)) }
                .map_err(failed("capture the monitor"))
        }
        Target::Window { window, .. } => {
            // SAFETY: as above, for a window handle.
            unsafe { interop.CreateForWindow(HWND(window as *mut c_void)) }
                .map_err(failed("capture the window"))
        }
    }
}

/// A flag Windows sets when it ends the capture of `item`, and the token that stops it from
/// being set.
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

/// The shared device, wrapped for the Windows Runtime capture API.
fn winrt_device(device: &Device) -> Result<IDirect3DDevice, CaptureError> {
    let dxgi = device
        .dxgi()
        .map_err(failed("share the device with capture"))?;
    // SAFETY: wraps the live device for the WinRT capture API.
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .and_then(|inspectable| inspectable.cast())
        .map_err(failed("share the device with capture"))
}

/// Starts capturing `item` into `pool`, with the cursor and without the border, at most twice
/// per `period`.
fn start_session(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    period: Duration,
) -> Result<GraphicsCaptureSession, CaptureError> {
    let session = pool
        .CreateCaptureSession(item)
        .map_err(failed("create the capture session"))?;
    // Each of these needs a newer Windows than capture itself; without them the defaults
    // stay: the cursor is captured anyway, the border shows, frames come at the display rate.
    let _ = session.SetIsCursorCaptureEnabled(true);
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetMinUpdateInterval(TimeSpan {
        Duration: i64::try_from((period / 2).as_nanos() / HUNDRED_NANOSECONDS).unwrap_or(i64::MAX),
    });
    session.StartCapture().map_err(failed("start capturing"))?;
    Ok(session)
}

/// The newest frame waiting in `pool`, if any; older ones are handed back unseen, since only
/// the newest matters. The pool reports having no frame as an error.
fn newest_frame(pool: &Direct3D11CaptureFramePool) -> Option<Direct3D11CaptureFrame> {
    let mut newest = None;
    while let Ok(frame) = pool.TryGetNextFrame() {
        if let Some(older) = newest.replace(frame) {
            let _ = older.Close();
        }
    }
    newest
}

/// The Direct3D 11 texture a captured frame's surface wraps.
fn frame_texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D, CaptureError> {
    frame
        .Surface()
        .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
        // SAFETY: the surface of a live frame wraps a Direct3D 11 texture.
        .and_then(|access| unsafe { access.GetInterface() })
        .map_err(failed("read the captured frame"))
}
