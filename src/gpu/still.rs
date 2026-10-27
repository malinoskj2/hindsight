//! One picture for a screenshot: copied out of a capture on the GPU and read back on the thread
//! that saves it, or grabbed from a whole monitor on a device of its own when no recording
//! shows it.

use crate::{
    config::Backend,
    gpu::{
        capture::{self, CopyTarget, Frame, Surface},
        cuda::{CudaError, DeviceBuffer},
        device::{Device, Locked, null_output},
    },
    log,
    screenshot::{Grab, Readback, Still, StillError, pitched_span},
    service::Target,
};
use std::{
    error::Error,
    ptr, slice, thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D9::{
                D3DFMT_A8R8G8B8, D3DLOCK_READONLY, D3DLOCKED_RECT, D3DMULTISAMPLE_NONE,
                D3DPOOL_SYSTEMMEM, D3DTEXF_NONE, IDirect3DDevice9, IDirect3DSurface9,
            },
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11DeviceContext, ID3D11Multithread,
                ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                DXGI_ERROR_WAS_STILL_DRAWING,
            },
        },
    },
    core::Interface,
};

const POLL: Duration = Duration::from_millis(1);
/// How long a new capture may take to deliver its first picture.
const FIRST_PICTURE: Duration = Duration::from_secs(1);
/// The update interval asked of Windows Graphics Capture. A grab takes one picture, so it only
/// bounds how soon the first arrives.
const GRAB_PERIOD: Duration = Duration::from_millis(16);

type Cause = Box<dyn Error + Send + Sync>;

/// Copies the top-left `width` × `height` of `surface` on the GPU, which the screenshot thread
/// then reads back. The capture thread only queues the copy, except on CUDA, where it waits
/// for the device-to-device copy before the grab may reuse the buffer.
pub(super) fn copy(
    device: &Device,
    surface: &Surface,
    width: u32,
    height: u32,
) -> Result<Box<dyn Readback>, StillError> {
    start(device, surface, width, height).map_err(StillError::Copy)
}

fn start(
    device: &Device,
    surface: &Surface,
    width: u32,
    height: u32,
) -> Result<Box<dyn Readback>, Cause> {
    let copy: Box<dyn Readback> = match surface {
        Surface::Texture(texture) => Box::new(TextureCopy::start(device, texture, width, height)?),
        Surface::Direct3D9(surface) => Box::new(Direct3D9Copy::start(surface, width, height)?),
        Surface::CudaBuffer { buffer, pitch } => {
            Box::new(CudaCopy::start(buffer, *pitch, width, height)?)
        }
    };
    Ok(copy)
}

/// Captures one picture of a whole monitor on the calling thread, on a device of its own,
/// tone-mapped on an HDR display as clips are.
pub(crate) fn grab(grab: &Grab) -> Result<Still, StillError> {
    let device = Device::create().map_err(grab_failed)?;
    let target = Target::Monitor {
        handle: grab.monitor,
        name: "the monitor in front".to_owned(),
    };
    let mut capture = start_grab(grab, &device, &target)?;
    let (width, height) = capture.size();
    let picture = picture_texture(&device, width, height).map_err(grab_failed)?;
    take_first(
        &mut capture,
        CopyTarget {
            device: &device,
            texture: &picture,
            width,
            height,
        },
    )?;
    drop(capture);
    let copy = TextureCopy::start(&device, &picture, width, height).map_err(copy_failed)?;
    Box::new(copy).finish()
}

/// Desktop Duplication when it is the backend chosen, and Windows Graphics Capture otherwise,
/// since NvFBC clips HDR and costs more to open for one picture. Each falls back on the other.
const fn grab_backends(chosen: Backend) -> [Backend; 2] {
    match chosen {
        Backend::Dxgi => [Backend::Dxgi, Backend::Wgc],
        Backend::Wgc | Backend::Nvfbc | Backend::NvfbcCuda => [Backend::Wgc, Backend::Dxgi],
    }
}

fn start_grab(
    grab: &Grab,
    device: &Device,
    target: &Target,
) -> Result<capture::Capture, StillError> {
    let [first, second] = grab_backends(grab.backend);
    capture::start(first, device, GRAB_PERIOD, target, grab.cursor)
        .or_else(|error| {
            log::warning(format!(
                "Screenshot · {first} unavailable ({error}) · trying {second}"
            ));
            capture::start(second, device, GRAB_PERIOD, target, grab.cursor)
        })
        .map_err(grab_failed)
}

fn take_first(capture: &mut capture::Capture, target: CopyTarget<'_>) -> Result<(), StillError> {
    let deadline = Instant::now() + FIRST_PICTURE;
    loop {
        match capture.take(Some(target)).map_err(grab_failed)? {
            Frame::Fresh => return Ok(()),
            Frame::Unchanged if Instant::now() < deadline => thread::sleep(POLL),
            Frame::Unchanged | Frame::Resized | Frame::InSurface { .. } => {
                return Err(StillError::NoPicture);
            }
        }
    }
}

/// A texture in the encoder's input format, which the capture copies or tone-maps into.
#[expect(unsafe_code, reason = "calls ID3D11Device::CreateTexture2D")]
fn picture_texture(
    device: &Device,
    width: u32,
    height: u32,
) -> windows::core::Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0.cast_unsigned(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: a plain texture description without initial data; the out-parameter is live.
    unsafe {
        device
            .d3d
            .CreateTexture2D(&raw const description, None, Some(&raw mut texture))
    }?;
    texture.ok_or_else(null_output)
}

fn grab_failed(error: impl Error + Send + Sync + 'static) -> StillError {
    StillError::Grab(Box::new(error))
}

fn copy_failed(error: impl Error + Send + Sync + 'static) -> StillError {
    StillError::Copy(Box::new(error))
}

fn read_failed(error: impl Error + Send + Sync + 'static) -> StillError {
    StillError::Read(Box::new(error))
}

/// A copy into a staging texture on the capture's Direct3D 11 device.
#[derive(Debug)]
struct TextureCopy {
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    lock: ID3D11Multithread,
    width: u32,
    height: u32,
}

#[expect(unsafe_code, reason = "calls Direct3D 11 to copy and map the picture")]
impl TextureCopy {
    fn start(
        device: &Device,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Self> {
        let mut source_description = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: describes a live texture into a description that outlives the call.
        unsafe { source.GetDesc(&raw mut source_description) };
        let description = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source_description.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: a plain texture description without initial data.
        unsafe {
            device
                .d3d
                .CreateTexture2D(&raw const description, None, Some(&raw mut staging))
        }?;
        let staging = staging.ok_or_else(null_output)?;
        let lock: ID3D11Multithread = device.context.cast()?;
        {
            let _locked = Locked::new(&lock);
            device.copy_region(&staging, source, width, height);
            // SAFETY: submits the queued copy; no preconditions.
            unsafe { device.context.Flush() };
        }
        Ok(Self {
            staging,
            context: device.context.clone(),
            lock,
            width,
            height,
        })
    }

    /// Polls with `DO_NOT_WAIT`, because a waiting `Map` would hold the device lock against the
    /// capture thread.
    fn map(&self) -> windows::core::Result<D3D11_MAPPED_SUBRESOURCE> {
        loop {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let result = {
                let _locked = Locked::new(&self.lock);
                // SAFETY: a staging texture made for CPU reads; `mapped` outlives the call.
                unsafe {
                    self.context.Map(
                        &self.staging,
                        0,
                        D3D11_MAP_READ,
                        D3D11_MAP_FLAG_DO_NOT_WAIT.0.cast_unsigned(),
                        Some(&raw mut mapped),
                    )
                }
            };
            match result {
                Ok(()) => return Ok(mapped),
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => thread::sleep(POLL),
                Err(error) => return Err(error),
            }
        }
    }
}

#[expect(unsafe_code, reason = "reads the mapped staging texture")]
impl Readback for TextureCopy {
    fn finish(self: Box<Self>) -> Result<Still, StillError> {
        let mapped = self.map().map_err(read_failed)?;
        let pitch = mapped.RowPitch as usize;
        let still = pitched_span(self.width, self.height, pitch).and_then(|span| {
            // SAFETY: while mapped, the staging texture's `height` rows lie `RowPitch` bytes
            // apart from `pData`, each at least `width` pixels long, so `span` bytes are
            // readable. They are copied out before the unmap below.
            let bytes = unsafe { slice::from_raw_parts(mapped.pData.cast::<u8>(), span) };
            Still::from_pitched(self.width, self.height, pitch, bytes)
        });
        {
            let _locked = Locked::new(&self.lock);
            // SAFETY: undoes the map above; nothing points into the mapping any more.
            unsafe { self.context.Unmap(&self.staging, 0) };
        }
        still.ok_or(StillError::Truncated {
            width: self.width,
            height: self.height,
        })
    }
}

/// A copy into a render target of its own on NvFBC's Direct3D 9 device, which the grabs never
/// draw into.
#[derive(Debug)]
struct Direct3D9Copy {
    device: IDirect3DDevice9,
    copy: IDirect3DSurface9,
    width: u32,
    height: u32,
}

#[expect(unsafe_code, reason = "declares the Direct3D 9 copy sendable")]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the device and surface are free-threaded, as the SAFETY comment says"
)]
// SAFETY: NvFBC's device is created with `D3DCREATE_MULTITHREADED`, so the device and its
// surfaces may be used and released from any thread.
unsafe impl Send for Direct3D9Copy {}

#[expect(unsafe_code, reason = "calls Direct3D 9 to copy and lock the picture")]
impl Direct3D9Copy {
    fn start(source: &IDirect3DSurface9, width: u32, height: u32) -> windows::core::Result<Self> {
        // SAFETY: a live surface; the device comes back with a reference of its own.
        let device = unsafe { source.GetDevice() }?;
        let mut copy = None;
        // SAFETY: an unshared render target, so the handle pointer may be null. The
        // out-parameter is written on success.
        unsafe {
            device.CreateRenderTarget(
                width,
                height,
                D3DFMT_A8R8G8B8,
                D3DMULTISAMPLE_NONE,
                0,
                false,
                &raw mut copy,
                ptr::null_mut(),
            )
        }?;
        let copy = copy.ok_or_else(null_output)?;
        let region = region(width, height)?;
        // SAFETY: two live surfaces of this device in NvFBC's format; the region lies within
        // each, so nothing is scaled.
        unsafe {
            device.StretchRect(
                source,
                &raw const region,
                &copy,
                &raw const region,
                D3DTEXF_NONE,
            )
        }?;
        Ok(Self {
            device,
            copy,
            width,
            height,
        })
    }

    fn read(&self) -> windows::core::Result<Option<Still>> {
        let mut memory = None;
        // SAFETY: an unshared surface in system memory, so the handle pointer may be null. The
        // out-parameter is written on success.
        unsafe {
            self.device.CreateOffscreenPlainSurface(
                self.width,
                self.height,
                D3DFMT_A8R8G8B8,
                D3DPOOL_SYSTEMMEM,
                &raw mut memory,
                ptr::null_mut(),
            )
        }?;
        let memory = memory.ok_or_else(null_output)?;
        // SAFETY: copies the render target into a system-memory surface of its size and
        // format, once the copy into it has finished.
        unsafe { self.device.GetRenderTargetData(&self.copy, &memory) }?;
        let mut locked = D3DLOCKED_RECT::default();
        // SAFETY: locks the whole surface for reading; `locked` outlives the call.
        unsafe {
            memory.LockRect(
                &raw mut locked,
                ptr::null(),
                D3DLOCK_READONLY.cast_unsigned(),
            )
        }?;
        let pitch = usize::try_from(locked.Pitch).ok();
        let still = pitch.and_then(|pitch| {
            let span = pitched_span(self.width, self.height, pitch)?;
            // SAFETY: while locked, the surface's `height` rows lie `Pitch` bytes apart from
            // `pBits`, each at least `width` pixels long, so `span` bytes are readable. They
            // are copied out before the unlock below.
            let bytes = unsafe { slice::from_raw_parts(locked.pBits.cast::<u8>(), span) };
            Still::from_pitched(self.width, self.height, pitch, bytes)
        });
        // SAFETY: undoes the lock above; nothing points into the surface any more.
        unsafe { memory.UnlockRect() }?;
        Ok(still)
    }
}

impl Readback for Direct3D9Copy {
    fn finish(self: Box<Self>) -> Result<Still, StillError> {
        self.read()
            .map_err(read_failed)?
            .ok_or(StillError::Truncated {
                width: self.width,
                height: self.height,
            })
    }
}

fn region(width: u32, height: u32) -> windows::core::Result<RECT> {
    Ok(RECT {
        left: 0,
        top: 0,
        right: i32::try_from(width)?,
        bottom: i32::try_from(height)?,
    })
}

/// A copy into a device buffer of its own in the grab's CUDA context.
#[derive(Debug)]
struct CudaCopy {
    buffer: DeviceBuffer,
    pitch: usize,
    width: u32,
    height: u32,
}

impl CudaCopy {
    fn start(source: &DeviceBuffer, pitch: u32, width: u32, height: u32) -> Result<Self, Cause> {
        let pitch = pitch as usize;
        let span =
            pitched_span(width, height, pitch).ok_or(StillError::Truncated { width, height })?;
        let _current = source.make_current()?;
        let buffer = source.allocate_beside(span)?;
        buffer.copy_from(0, source, 0, span)?;
        source.synchronize()?;
        Ok(Self {
            buffer,
            pitch,
            width,
            height,
        })
    }

    fn read(&self) -> Result<Vec<u8>, CudaError> {
        let _current = self.buffer.make_current()?;
        let mut bytes = vec![0; self.buffer.bytes()];
        self.buffer.copy_to_host(0, &mut bytes)?;
        Ok(bytes)
    }
}

impl Readback for CudaCopy {
    fn finish(self: Box<Self>) -> Result<Still, StillError> {
        let bytes = self.read().map_err(read_failed)?;
        Still::from_pitched(self.width, self.height, self.pitch, &bytes).ok_or(
            StillError::Truncated {
                width: self.width,
                height: self.height,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grab_prefers_the_chosen_backend_unless_it_is_nvfbc() {
        assert_eq!(grab_backends(Backend::Dxgi), [Backend::Dxgi, Backend::Wgc]);
        for backend in [Backend::Wgc, Backend::Nvfbc, Backend::NvfbcCuda] {
            assert_eq!(grab_backends(backend), [Backend::Wgc, Backend::Dxgi]);
        }
    }
}
