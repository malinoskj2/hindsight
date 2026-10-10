//! Windows Graphics Capture of the primary monitor. Frames land in a free-threaded frame pool;
//! on each tick the capture thread takes the newest one, copies it into the encoder's input
//! texture on the GPU and hands the pool's buffer straight back to Windows.

use super::device::Device;
use std::time::Duration;
use thiserror::Error;
use windows::{
    Foundation::TimeSpan,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::POINT,
        Graphics::{
            Direct3D11::{D3D11_BOX, ID3D11Texture2D},
            Dxgi::IDXGIDevice,
            Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint},
        },
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

#[derive(Debug, Error)]
pub(crate) enum CaptureError {
    #[error(
        "Windows Graphics Capture is not available; Windows 10 version 1903 or newer is needed"
    )]
    Unsupported,
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
}

/// What a tick found.
pub(super) enum Frame {
    /// A new picture was copied into the target.
    Fresh,
    /// The screen has not changed since the last tick.
    Unchanged,
    /// The monitor's resolution changed.
    Resized,
}

/// Joins the calling thread to the multithreaded apartment the free-threaded capture objects
/// live in; a thread already in it stays.
pub(super) fn join_apartment() {
    // SAFETY: initializes WinRT for the calling thread only.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

pub(super) struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
}

impl Capture {
    /// Starts capturing the primary monitor, with the cursor, at most once per `period`.
    pub(super) fn primary_monitor(device: &Device, period: Duration) -> Result<Self, CaptureError> {
        let failed = |step| move |error| CaptureError::Windows(step, error);
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::Unsupported);
        }

        // SAFETY: (0, 0) always lies on the primary monitor.
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(failed("open the capture interop"))?;
        // SAFETY: `monitor` is a live monitor handle.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }
            .map_err(failed("capture the primary monitor"))?;
        let size = item.Size().map_err(failed("read the monitor size"))?;

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
            Duration: i64::try_from(period.as_nanos() / 100).unwrap_or(i64::MAX),
        });
        session.StartCapture().map_err(failed("start capturing"))?;

        Ok(Self {
            pool,
            session,
            size,
        })
    }

    /// The monitor's size in pixels.
    pub(super) fn size(&self) -> (u32, u32) {
        (
            self.size.Width.unsigned_abs(),
            self.size.Height.unsigned_abs(),
        )
    }

    /// Copies the newest captured frame into the top-left `width` × `height` of `target`, if
    /// one arrived since the last call.
    pub(super) fn take(
        &self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        let failed = |step| move |error| CaptureError::Windows(step, error);

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
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both textures are BGRA on the same device and the region lies within each.
        unsafe {
            device.context.CopySubresourceRegion(
                target,
                0,
                0,
                0,
                0,
                &texture,
                0,
                Some(&raw const region),
            );
        }
        // Returns the buffer to the pool; the copy above is already queued on the GPU.
        let _ = frame.Close();
        Ok(Frame::Fresh)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
