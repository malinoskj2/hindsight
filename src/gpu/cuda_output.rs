//! The replay buffer in CUDA memory, written by NVENC itself, so that packets never cross to
//! system memory.

use crate::{
    gpu::cuda::{ContextHold, CudaError, Device, DeviceBuffer, DevicePointer, Driver},
    log,
    replay::{
        BufferTooLarge, CHUNK_BYTES, PAGE_BYTES, Page, Pages, Slot, StoreError, pages_within,
        pieces,
    },
};
use std::{mem, sync::Arc};
use thiserror::Error;

/// The layout of a frame the encoder writes to a staging buffer: a header, then the bitstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StagingLayout {
    pub(super) header_bytes: usize,
    /// The offset of the bitstream's size within the header, stored as a little-endian `u32`.
    pub(super) size_offset: usize,
    /// The number of bitstream bytes read back with the header, enough to recognise a keyframe.
    pub(super) head_bytes: usize,
}

#[derive(Debug, Error)]
pub(super) enum CudaRingError {
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    #[error(transparent)]
    TooLarge(#[from] BufferTooLarge),
}

#[derive(Debug, Error)]
#[error(
    "the frame's header claims {size} bytes of bitstream, more than the {room} its buffer holds"
)]
struct Oversized {
    size: usize,
    room: usize,
}

/// A staging buffer of the ring, identified by its allocation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StagingId(usize);

pub(super) struct CudaRing {
    /// The primary context, released once the last page is dropped.
    hold: Arc<ContextHold>,
    pages: Pages<CudaPage>,
    staging: Vec<DeviceBuffer>,
    layout: StagingLayout,
    head: Box<[u8]>,
}

impl CudaRing {
    /// Creates the ring. The primary context of `device` must be current on this thread.
    pub(super) fn create(
        device: Device,
        bytes: u64,
        layout: StagingLayout,
    ) -> Result<Self, CudaRingError> {
        let driver =
            Driver::load().map_err(|error| CudaRingError::Cuda("load the CUDA driver", error))?;
        let hold = Arc::new(
            driver
                .retain_primary_context(device)
                .map_err(|error| CudaRingError::Cuda("retain the CUDA context", error))?,
        );
        let (_, available) = hold
            .driver()
            .memory_info()
            .map_err(|error| CudaRingError::Cuda("read the GPU's memory size", error))?;
        let count = pages_within(bytes, available)?;
        let pages = Pages::reserve(PAGE_BYTES, count, || CudaPage::allocate(&hold))
            .map_err(|error| CudaRingError::Cuda("reserve a page of GPU memory", error))?;
        Ok(Self {
            hold,
            pages,
            staging: Vec::new(),
            layout,
            head: vec![0; layout.header_bytes + layout.head_bytes].into_boxed_slice(),
        })
    }

    pub(super) fn description(&self) -> String {
        format!("Buffer · CUDA memory · {}", self.pages.summary())
    }

    /// Allocates a buffer for the encoder to write frames into, and returns its id and device
    /// address.
    pub(super) fn allocate_staging(
        &mut self,
        bytes: usize,
    ) -> Result<(StagingId, DevicePointer), CudaError> {
        debug_assert!(
            bytes >= self.head.len(),
            "a staging buffer holds a frame's head"
        );
        let buffer = self.hold.allocate(bytes)?;
        let pointer = buffer.pointer();
        self.staging.push(buffer);
        Ok((StagingId(self.staging.len() - 1), pointer))
    }

    /// Leaks the staging buffers rather than freeing memory a stalled encoder may still write
    /// to. The context stays held with them for the rest of the process.
    pub(super) fn leak_staging(&mut self) {
        if self.staging.is_empty() {
            return;
        }
        log::warning(format!(
            "Encoder stalled · {} output buffers left in GPU memory",
            self.staging.len()
        ));
        for buffer in self.staging.drain(..) {
            mem::forget(buffer);
        }
    }

    /// Moves the frame the encoder finished in `staging` into the ring.
    pub(super) fn take(&mut self, staging: StagingId) -> Result<(Slot, Box<[u8]>), StoreError> {
        let Self {
            hold,
            pages,
            staging: buffers,
            layout,
            head,
        } = self;
        let driver = hold.driver();
        let StagingLayout {
            header_bytes,
            size_offset,
            head_bytes,
        } = *layout;
        let staging = &buffers[staging.0];
        let room = staging.bytes().saturating_sub(header_bytes);
        staging
            .copy_to_host(0, head)
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
        page.0
            .copy_from(offset, staging, header_bytes, size)
            .map_err(StoreError::call("move a frame into the buffer"))?;
        // The copy must be done before the encoder writes the staging buffer again.
        driver
            .synchronize()
            .map_err(StoreError::call("wait for a frame's move"))?;
        Ok((Slot::new(page, offset, size), head))
    }
}

fn bitstream_size(staging: &[u8], size_offset: usize) -> usize {
    let bytes = staging[size_offset..size_offset + size_of::<u32>()]
        .try_into()
        .expect("the slice is four bytes");
    u32::from_le_bytes(bytes) as usize
}

struct CudaPage(DeviceBuffer);

impl CudaPage {
    fn allocate(hold: &Arc<ContextHold>) -> Result<Self, CudaError> {
        hold.allocate(PAGE_BYTES).map(Self)
    }
}

impl Page for CudaPage {
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        // A save reads on a thread of its own, where the context is not current.
        let _current = self
            .0
            .make_current()
            .map_err(StoreError::call("make the CUDA context current"))?;
        let mut chunk = vec![0; length.min(CHUNK_BYTES)];
        for (done, piece) in pieces(length, chunk.len()) {
            self.0
                .copy_to_host(offset + done, &mut chunk[..piece])
                .map_err(StoreError::call("read a page back"))?;
            sink(done, &chunk[..piece]);
        }
        Ok(())
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
