//! The replay buffer in CUDA memory, written by NVENC itself: packets never cross to system memory.

use super::{
    CHUNK_BYTES, MEBIBYTE, PAGE_BYTES,
    cuda::{Context, CudaError, Device, DevicePointer, Driver},
    reserved_pages,
};
use crate::replay::{Page, Pages, Slot, StoreError};
use std::sync::Arc;
use thiserror::Error;

/// Bitstream bytes read back with a frame's header, enough to tell a keyframe.
pub(super) const HEAD_BYTES: usize = 16 << 10;

#[derive(Debug, Error)]
pub(crate) enum CudaRingError {
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    #[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
    TooLarge { needed: usize, available: usize },
}

#[derive(Debug, Error)]
enum TakeError {
    #[error("the encoder completed a frame in a buffer the ring did not allocate")]
    UnknownStaging,
    #[error(
        "the frame's header claims {size} bytes of bitstream, more than the {room} its buffer holds"
    )]
    Oversized { size: usize, room: usize },
}

struct Staging {
    pointer: DevicePointer,
    bytes: usize,
}

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

pub(super) struct CudaRing {
    driver: Arc<Driver>,
    hold: Arc<ContextHold>,
    pages: Pages<CudaPage>,
    staging: Vec<Staging>,
    head: Box<[u8]>,
    reserved: usize,
}

impl CudaRing {
    /// The primary context of `device` must be current on this thread.
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

    pub(super) fn description(&self) -> String {
        format!(
            "Replay buffer in CUDA memory, written by the encoder: {} MiB reserved in {} MiB pages",
            self.reserved * PAGE_BYTES / MEBIBYTE,
            PAGE_BYTES / MEBIBYTE
        )
    }

    pub(super) fn allocate_staging(&mut self, bytes: usize) -> Result<DevicePointer, CudaError> {
        debug_assert!(
            bytes >= self.head.len(),
            "a staging buffer holds a frame's head"
        );
        let pointer = self.driver.allocate(bytes)?;
        self.staging.push(Staging { pointer, bytes });
        Ok(pointer)
    }

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
        // Allocated in the ring's context, which need not be current here.
        if let Ok(_current) = Current::push(&self.driver, self.hold.context) {
            for buffer in self.staging.drain(..) {
                let _ = self.driver.free(buffer.pointer);
            }
        }
    }
}

fn bitstream_size(staging: &[u8]) -> usize {
    let bytes = staging[SIZE_OFFSET..SIZE_OFFSET + size_of::<u32>()]
        .try_into()
        .expect("the slice is four bytes");
    u32::from_le_bytes(bytes) as usize
}

const HEADER_BYTES: usize = super::nvenc::ENCODE_OUT_PARAMS_BYTES;
const SIZE_OFFSET: usize = super::nvenc::ENCODE_OUT_PARAMS_SIZE_OFFSET;

struct CudaPage {
    pointer: DevicePointer,
    driver: Arc<Driver>,
    hold: Arc<ContextHold>,
}

impl CudaPage {
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
        staging[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        staging[HEADER_BYTES] = 0xFF;
        assert_eq!(bitstream_size(&staging), 0x0001_0203);
    }
}
