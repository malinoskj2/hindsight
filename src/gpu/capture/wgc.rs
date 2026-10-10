//! Windows Graphics Capture of a monitor or a window. Frames land in a free-threaded frame pool;
//! on each tick the capture thread takes the newest one, copies it into the encoder's input
//! texture on the GPU and hands the pool's buffer straight back to Windows.

use super::{CaptureError, Frame, Source, monitor};
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
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::HWND,
        Graphics::{Direct3D11::ID3D11Texture2D, Dxgi::IDXGIDevice},
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

pub(super) struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    window: bool,
    /// Set when Windows ends the capture: the monitor was unplugged, the window closed or the
    /// device was lost.
    closed: Arc<AtomicBool>,
    closed_token: i64,
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
        let failed = |step| move |error| CaptureError::Windows(step, error);
        // The free-threaded capture objects live in the multithreaded apartment; a thread
        // already in it stays.
        // SAFETY: initializes WinRT for the calling thread only.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported(
                "Windows Graphics Capture is not available; Windows 10 version 1903 or newer is needed"
                    .to_owned(),
            ));
        }

        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(failed("open the capture interop"))?;
        let item: GraphicsCaptureItem = match *target {
            Target::Monitor { handle, .. } => {
                // SAFETY: the handle was read from the window being captured moments ago, or
                // names the primary monitor; a stale one makes the call fail, not misbehave.
                unsafe { interop.CreateForMonitor(monitor(handle)) }
                    .map_err(failed("capture the monitor"))?
            }
            Target::Window { window, .. } => {
                // SAFETY: as above, for a window handle.
                unsafe { interop.CreateForWindow(HWND(window as *mut c_void)) }
                    .map_err(failed("capture the window"))?
            }
        };
        let size = item.Size().map_err(failed("read the capture size"))?;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let closed_token = item
            .Closed(&TypedEventHandler::new(move |_, _| {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            }))
            .map_err(failed("watch the capture item"))?;

        let dxgi: IDXGIDevice = device
            .d3d
            .cast()
            .map_err(failed("share the device with capture"))?;
        // SAFETY: wraps the live device for the WinRT capture API.
        let direct3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|inspectable| inspectable.cast())
            .map_err(failed("share the device with capture"))?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            size,
        )
        .map_err(failed("create the capture frame pool"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(failed("create the capture session"))?;
        // Each of these needs a newer Windows than capture itself; without them the defaults
        // stay: the cursor is captured anyway, the border shows, frames come at the display rate.
        let _ = session.SetIsCursorCaptureEnabled(true);
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetMinUpdateInterval(TimeSpan {
            Duration: i64::try_from((period / 2).as_nanos() / HUNDRED_NANOSECONDS)
                .unwrap_or(i64::MAX),
        });
        session.StartCapture().map_err(failed("start capturing"))?;

        Ok(Self {
            item,
            pool,
            session,
            size,
            window: matches!(target, Target::Window { .. }),
            closed,
            closed_token,
        })
    }
}

impl Source for Capture {
    fn size(&self) -> (u32, u32) {
        let pixels = |length| u32::try_from(length).expect("a capture size is never negative");
        (pixels(self.size.Width), pixels(self.size.Height))
    }

    fn captures_window(&self) -> bool {
        self.window
    }

    fn take(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        let failed = |step| move |error| CaptureError::Windows(step, error);
        if self.closed.load(Ordering::Relaxed) {
            return Err(CaptureError::Closed);
        }

        // Older frames are dropped unseen; only the newest matters. No frame reads as an error.
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
            }
        }
        let Some(frame) = newest else {
            return Ok(Frame::Unchanged);
        };

        let content = frame.ContentSize().map_err(failed("read the frame size"))?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            let _ = frame.Close();
            return Ok(Frame::Resized);
        }

        let texture: ID3D11Texture2D = frame
            .Surface()
            .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
            // SAFETY: the surface of a live frame wraps a Direct3D 11 texture.
            .and_then(|access| unsafe { access.GetInterface() })
            .map_err(failed("read the captured frame"))?;
        device.copy_region(target, &texture, width, height);
        // Returns the buffer to the pool; the copy above is already queued on the GPU.
        let _ = frame.Close();
        Ok(Frame::Fresh)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
