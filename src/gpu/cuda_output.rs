//! The replay buffer in CUDA memory, filled by the encoder itself. On a CUDA session NVENC can
//! write each frame's bitstream into a buffer of the application's in video memory instead
//! of its own: the encoder registers a few such staging buffers, one per slot, and after a
//! frame completes the packet is moved from its staging buffer into 64 MiB pages with a
//! device-to-device copy. Only the frame's header (its size, then enough of the bitstream to
//! tell a keyframe) crosses to system memory, so no packet is ever copied on the CPU, and
//! there is no second Direct3D device with a driver thread of its own, as the Direct3D 11
//! store ([`super::vram`]) has.
//!
//! The pages are allocated in the device's primary context, which the ring retains itself,
//! so they outlive the capture session that the encoder runs on and stay readable until the
//! buffer is dropped. A save reads them back 32 MiB at a time on its own thread, with the
//! context made current there for the copies.

use super::{
    CHUNK_BYTES, MEBIBYTE, PAGE_BYTES,
    cuda::{Context, CudaError, Device, DevicePointer, Driver},
    reserved_pages,
};
use crate::replay::{Page, Pages, Slot, StoreError};
use std::sync::Arc;
use thiserror::Error;

/// Bytes of a frame's bitstream read back with its header, enough to tell a keyframe from
/// its first headers; the whole packet stays in video memory.
pub(super) const HEAD_BYTES: usize = 16 << 10;

/// Why the buffer could not be placed in CUDA memory.
#[derive(Debug, Error)]
pub(crate) enum CudaRingError {
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    /// The buffer is larger than the GPU's memory altogether.
    #[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
    TooLarge { needed: usize, available: usize },
}

/// Why a completed frame could not be taken from its staging buffer: what the encoder wrote
/// there does not add up, which a wrong bit in the session's setup would cause.
#[derive(Debug, Error)]
enum TakeError {
    #[error("the encoder completed a frame in a buffer the ring did not allocate")]
    UnknownStaging,
    /// The header claims more bitstream than the buffer holds, so the frame cannot be real.
    #[error(
        "the frame's header claims {size} bytes of bitstream, more than the {room} its buffer holds"
    )]
    Oversized { size: usize, room: usize },
}

/// A buffer the encoder writes frames to, and how many bytes it holds.
struct Staging {
    pointer: DevicePointer,
    bytes: usize,
}

/// A reference on a device's primary context, given back when the last holder goes.
struct ContextHold {
    driver: Arc<Driver>,
    device: Device,
    context: Context,
}

impl Drop for ContextHold {
    fn drop(&mut self) {
        let _ = self.driver.release_primary_context(self.device);
    }
}

/// The buffer's pages in CUDA memory, the encoder's staging buffers, and what they are kept
/// in. Used on the capture thread, where the session's context is current.
pub(super) struct CudaRing {
    driver: Arc<Driver>,
    hold: Arc<ContextHold>,
    pages: Pages<CudaPage>,
    /// The output buffers the encoder writes frames to, freed with the ring.
    staging: Vec<Staging>,
    /// The frame's header and the start of its bitstream, read back for each frame.
    head: Box<[u8]>,
    /// Pages reserved up front.
    reserved: usize,
}

impl CudaRing {
    /// Reserves pages for `bytes` of packets, and one more for the group of pictures the
    /// buffer keeps beyond its span, in the primary context of `device`, which must be the
    /// context current on this thread.
    pub(super) fn create(device: Device, bytes: u64) -> Result<Self, CudaRingError> {
        let driver = Arc::new(
            Driver::load().map_err(|error| CudaRingError::Cuda("load the CUDA driver", error))?,
        );
        let context = driver
            .retain_primary_context(device)
            .map_err(|error| CudaRingError::Cuda("retain the CUDA context", error))?;
        let hold = Arc::new(ContextHold {
            driver: Arc::clone(&driver),
            device,
            context,
        });
        let pages = reserved_pages(bytes);
        let needed = pages.saturating_mul(PAGE_BYTES);
        let (_, available) = driver
            .memory_info()
            .map_err(|error| CudaRingError::Cuda("read the GPU's memory size", error))?;
        if needed > available {
            return Err(CudaRingError::TooLarge { needed, available });
        }
        let reserved = (0..pages)
            .map(|_| CudaPage::allocate(&driver, &hold))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CudaRingError::Cuda("reserve a page of GPU memory", error))?;
        Ok(Self {
            driver,
            hold,
            reserved: reserved.len(),
            pages: Pages::new(PAGE_BYTES, reserved),
            staging: Vec::new(),
            head: vec![0; HEADER_BYTES + HEAD_BYTES].into_boxed_slice(),
        })
    }

    /// What was reserved, for the log.
    pub(super) fn description(&self) -> String {
        format!(
            "Replay buffer in CUDA memory, written by the encoder: {} MiB reserved in {} MiB pages",
            self.reserved * PAGE_BYTES / MEBIBYTE,
            PAGE_BYTES / MEBIBYTE
        )
    }

    /// Allocates a buffer of `bytes`, at least the header and the [`HEAD_BYTES`] read back
    /// with it, for the encoder to write frames to; it is freed with the ring, after the
    /// encoder has let go of it.
    pub(super) fn allocate_staging(&mut self, bytes: usize) -> Result<DevicePointer, CudaError> {
        debug_assert!(
            bytes >= self.head.len(),
            "a staging buffer holds a frame's head"
        );
        let pointer = self.driver.allocate(bytes)?;
        self.staging.push(Staging { pointer, bytes });
        Ok(pointer)
    }

    /// Moves the frame the encoder completed in the staging buffer at `staging` into the
    /// pages, and returns where it went with the start of its bitstream. On return the
    /// staging buffer may be written again.
    pub(super) fn take(&mut self, staging: DevicePointer) -> Result<(Slot, Box<[u8]>), StoreError> {
        let cuda = StoreError::call;
        let room = self
            .staging
            .iter()
            .find(|buffer| buffer.pointer == staging)
            .map(|buffer| buffer.bytes.saturating_sub(HEADER_BYTES))
            .ok_or(TakeError::UnknownStaging)
            .map_err(StoreError::call("find a frame's output buffer"))?;
        let Self {
            driver,
            hold,
            pages,
            head,
            ..
        } = self;
        driver
            .copy_to_host(head, staging)
            .map_err(cuda("read a frame's header back"))?;
        let size = bitstream_size(head);
        if size > room {
            let oversized = TakeError::Oversized { size, room };
            return Err(StoreError::call("check a frame's size")(oversized));
        }
        let head: Box<[u8]> = head[HEADER_BYTES..HEADER_BYTES + size.min(HEAD_BYTES)].into();

        let (page, offset) = pages.place(size, || {
            CudaPage::allocate(driver, hold).map_err(cuda("reserve a page of GPU memory"))
        })?;
        driver
            .copy_on_device(
                page.pointer + offset as u64,
                staging + HEADER_BYTES as u64,
                size,
            )
            .map_err(cuda("move a frame into the buffer"))?;
        // The copy must be done before the encoder writes the staging buffer again.
        driver
            .synchronize()
            .map_err(cuda("wait for a frame's move"))?;
        Ok((Pages::slot(page, offset, size), head))
    }
}

impl Drop for CudaRing {
    fn drop(&mut self) {
        // The encoder unregistered the staging buffers before dropping the ring. They were
        // allocated in the ring's context, which need not be current on the dropping thread.
        if let Ok(_current) = Current::push(&self.driver, self.hold.context) {
            for buffer in self.staging.drain(..) {
                let _ = self.driver.free(buffer.pointer);
            }
        }
        // The pages go with their last slot; the context after them.
    }
}

/// The size of the bitstream that follows the header the encoder wrote at the start of
/// `staging`, the first bytes of a completed staging buffer.
fn bitstream_size(staging: &[u8]) -> usize {
    let bytes = staging[SIZE_OFFSET..SIZE_OFFSET + size_of::<u32>()]
        .try_into()
        .expect("the slice is four bytes");
    u32::from_le_bytes(bytes) as usize
}

/// Bytes of `NV_ENC_ENCODE_OUT_PARAMS` in front of the bitstream in a staging buffer.
const HEADER_BYTES: usize = super::nvenc::ENCODE_OUT_PARAMS_BYTES;
/// Where the bitstream's size lies in that header.
const SIZE_OFFSET: usize = super::nvenc::ENCODE_OUT_PARAMS_SIZE_OFFSET;

/// One page: a block of device memory in the primary context.
struct CudaPage {
    pointer: DevicePointer,
    driver: Arc<Driver>,
    hold: Arc<ContextHold>,
}

impl CudaPage {
    /// A page in the context current on this thread, which is `hold`'s.
    fn allocate(driver: &Arc<Driver>, hold: &Arc<ContextHold>) -> Result<Self, CudaError> {
        Ok(Self {
            pointer: driver.allocate(PAGE_BYTES)?,
            driver: Arc::clone(driver),
            hold: Arc::clone(hold),
        })
    }
}

impl Page for CudaPage {
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        let cuda = StoreError::call;
        // A save reads on a thread of its own, where the context is not current.
        let _current = Current::push(&self.driver, self.hold.context)
            .map_err(cuda("make the CUDA context current"))?;
        let mut chunk = vec![0; length.min(CHUNK_BYTES)];
        let mut done = 0;
        while done < length {
            let piece = chunk.len().min(length - done);
            self.driver
                .copy_to_host(&mut chunk[..piece], self.pointer + (offset + done) as u64)
                .map_err(cuda("read a page back"))?;
            sink(done, &chunk[..piece]);
            done += piece;
        }
        Ok(())
    }
}

impl Drop for CudaPage {
    fn drop(&mut self) {
        // The last slot of a page may go on any thread.
        if let Ok(_current) = Current::push(&self.driver, self.hold.context) {
            let _ = self.driver.free(self.pointer);
        }
    }
}

/// A context made current on this thread while this lives.
struct Current<'a>(&'a Driver);

impl<'a> Current<'a> {
    fn push(driver: &'a Driver, context: Context) -> Result<Self, CudaError> {
        driver.push_context(context)?;
        Ok(Self(driver))
    }
}

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let _ = self.0.pop_context();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bitstream_size_is_read_from_the_header() {
        let mut staging = vec![0; HEADER_BYTES + HEAD_BYTES];
        staging[SIZE_OFFSET..SIZE_OFFSET + 4].copy_from_slice(&0x0001_0203_u32.to_le_bytes());
        assert_eq!(bitstream_size(&staging), 0x0001_0203);
        // The version word before it, and the bitstream after the header, are not the size.
        staging[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        staging[HEADER_BYTES] = 0xFF;
        assert_eq!(bitstream_size(&staging), 0x0001_0203);
    }
}
