//! The replay buffer in CUDA memory, written by NVENC itself: packets never cross to system memory.

use crate::{
    gpu::cuda::{Context, CudaError, Device, DevicePointer, Driver},
    replay::{CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, Page, Pages, Slot, StoreError, reserved_pages},
};
use std::sync::Arc;
use thiserror::Error;

/// How the encoder lays a frame out in a staging buffer: a header, then the bitstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StagingLayout {
    pub(super) header_bytes: usize,
    /// Of the bitstream's size in the header, as a little-endian `u32`.
    pub(super) size_offset: usize,
    /// Bitstream bytes read back with the header, enough to tell a keyframe.
    pub(super) head_bytes: usize,
}

#[derive(Debug, Error)]
pub(super) enum CudaRingError {
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    #[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
    TooLarge { needed: usize, available: usize },
}

#[derive(Debug, Error)]
#[error(
    "the frame's header claims {size} bytes of bitstream, more than the {room} its buffer holds"
)]
struct Oversized {
    size: usize,
    room: usize,
}

/// A staging buffer of the ring's, by the order it was allocated in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StagingId(usize);

struct Staging {
    pointer: DevicePointer,
    bytes: usize,
}

/// The driver and the primary context retained on it, released once the last page is gone.
struct ContextHold {
    driver: Driver,
    device: Device,
    context: Context,
}

impl Drop for ContextHold {
    fn drop(&mut self) {
        let _ = self.driver.release_primary_context(self.device);
    }
}

pub(super) struct CudaRing {
    hold: Arc<ContextHold>,
    pages: Pages<CudaPage>,
    staging: Vec<Staging>,
    layout: StagingLayout,
    head: Box<[u8]>,
    reserved: usize,
}

impl CudaRing {
    /// The primary context of `device` must be current on this thread.
    pub(super) fn create(
        device: Device,
        bytes: u64,
        layout: StagingLayout,
    ) -> Result<Self, CudaRingError> {
        let driver =
            Driver::load().map_err(|error| CudaRingError::Cuda("load the CUDA driver", error))?;
        let context = driver
            .retain_primary_context(device)
            .map_err(|error| CudaRingError::Cuda("retain the CUDA context", error))?;
        let hold = Arc::new(ContextHold {
            driver,
            device,
            context,
        });
        let pages = reserved_pages(bytes);
        let needed = pages.saturating_mul(PAGE_BYTES);
        let (_, available) = hold
            .driver
            .memory_info()
            .map_err(|error| CudaRingError::Cuda("read the GPU's memory size", error))?;
        if needed > available {
            return Err(CudaRingError::TooLarge { needed, available });
        }
        let reserved = (0..pages)
            .map(|_| CudaPage::allocate(&hold))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CudaRingError::Cuda("reserve a page of GPU memory", error))?;
        Ok(Self {
            hold,
            reserved: reserved.len(),
            pages: Pages::new(PAGE_BYTES, reserved),
            staging: Vec::new(),
            layout,
            head: vec![0; layout.header_bytes + layout.head_bytes].into_boxed_slice(),
        })
    }

    pub(super) fn description(&self) -> String {
        format!(
            "Replay buffer in CUDA memory, written by the encoder: {} MiB reserved in {} MiB pages",
            self.reserved * PAGE_BYTES / MEBIBYTE,
            PAGE_BYTES / MEBIBYTE
        )
    }

    /// A buffer for the encoder to write frames into, and where it is.
    pub(super) fn allocate_staging(
        &mut self,
        bytes: usize,
    ) -> Result<(StagingId, DevicePointer), CudaError> {
        debug_assert!(
            bytes >= self.head.len(),
            "a staging buffer holds a frame's head"
        );
        let pointer = self.hold.driver.allocate(bytes)?;
        self.staging.push(Staging { pointer, bytes });
        Ok((StagingId(self.staging.len() - 1), pointer))
    }

    /// Moves the frame the encoder finished in `staging` into the ring.
    pub(super) fn take(&mut self, staging: StagingId) -> Result<(Slot, Box<[u8]>), StoreError> {
        let Self {
            hold,
            pages,
            layout,
            head,
            ..
        } = self;
        let driver = &hold.driver;
        let StagingLayout {
            header_bytes,
            size_offset,
            head_bytes,
        } = *layout;
        let Staging { pointer, bytes } = self.staging[staging.0];
        let room = bytes.saturating_sub(header_bytes);
        driver
            .copy_to_host(head, pointer)
            .map_err(StoreError::call("read a frame's header back"))?;
        let size = bitstream_size(head, size_offset);
        if size > room {
            return Err(StoreError::call("check a frame's size")(Oversized {
                size,
                room,
            }));
        }
        let head: Box<[u8]> = head[header_bytes..header_bytes + size.min(head_bytes)].into();

        let (page, offset) = pages.place(size, || {
            CudaPage::allocate(hold).map_err(StoreError::call("reserve a page of GPU memory"))
        })?;
        driver
            .copy_on_device(
                page.pointer.offset(offset),
                pointer.offset(header_bytes),
                size,
            )
            .map_err(StoreError::call("move a frame into the buffer"))?;
        // The copy must be done before the encoder writes the staging buffer again.
        driver
            .synchronize()
            .map_err(StoreError::call("wait for a frame's move"))?;
        Ok((Slot::new(page, offset, size), head))
    }
}

impl Drop for CudaRing {
    fn drop(&mut self) {
        // Allocated in the ring's context, which need not be current here.
        if let Ok(_current) = Current::push(&self.hold.driver, self.hold.context) {
            for buffer in self.staging.drain(..) {
                let _ = self.hold.driver.free(buffer.pointer);
            }
        }
    }
}

fn bitstream_size(staging: &[u8], size_offset: usize) -> usize {
    let bytes = staging[size_offset..size_offset + size_of::<u32>()]
        .try_into()
        .expect("the slice is four bytes");
    u32::from_le_bytes(bytes) as usize
}

struct CudaPage {
    pointer: DevicePointer,
    hold: Arc<ContextHold>,
}

impl CudaPage {
    fn allocate(hold: &Arc<ContextHold>) -> Result<Self, CudaError> {
        Ok(Self {
            pointer: hold.driver.allocate(PAGE_BYTES)?,
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
        let driver = &self.hold.driver;
        // A save reads on a thread of its own, where the context is not current.
        let _current = Current::push(driver, self.hold.context)
            .map_err(StoreError::call("make the CUDA context current"))?;
        let mut chunk = vec![0; length.min(CHUNK_BYTES)];
        let mut done = 0;
        while done < length {
            let piece = chunk.len().min(length - done);
            driver
                .copy_to_host(&mut chunk[..piece], self.pointer.offset(offset + done))
                .map_err(StoreError::call("read a page back"))?;
            sink(done, &chunk[..piece]);
            done += piece;
        }
        Ok(())
    }
}

impl Drop for CudaPage {
    fn drop(&mut self) {
        // The last slot of a page may go on any thread.
        if let Ok(_current) = Current::push(&self.hold.driver, self.hold.context) {
            let _ = self.hold.driver.free(self.pointer);
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
        const HEADER_BYTES: usize = 16;
        const SIZE_OFFSET: usize = 4;
        let mut staging = vec![0; HEADER_BYTES + 8];
        staging[SIZE_OFFSET..SIZE_OFFSET + 4].copy_from_slice(&0x0001_0203_u32.to_le_bytes());
        assert_eq!(bitstream_size(&staging, SIZE_OFFSET), 0x0001_0203);
        staging[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        staging[HEADER_BYTES] = 0xFF;
        assert_eq!(bitstream_size(&staging, SIZE_OFFSET), 0x0001_0203);
    }
}
