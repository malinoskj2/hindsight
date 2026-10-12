//! The replay buffer in the GPU's own memory. Packets are uploaded, one after another, into
//! 64 MiB pages: Direct3D 11 buffers on a device of their own on the NVIDIA adapter, so a
//! save's read-backs never queue behind the capture's work on the shared device. A page is
//! reused once the buffer has dropped every packet in it, unless a save is still reading it,
//! in which case it is destroyed when the save is done and a fresh page takes its place. A
//! save reads a page's packets back in runs, through a staging buffer, 32 MiB at a time.
//!
//! What this costs: one small upload per frame on the capture thread, and the whole buffer
//! crossing the bus back into system memory when a clip is saved. What it buys: the system's
//! memory stays free of the buffer while the game runs.

use super::device::{Device, DeviceError};
use crate::replay::{Page, Slot, Store, StoreError};
use std::{
    collections::VecDeque,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use thiserror::Error;
use windows::Win32::Graphics::{
    Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
        D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_DEFAULT,
        D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    },
    Dxgi::DXGI_ERROR_WAS_STILL_DRAWING,
};

/// Bytes in a page. A packet never straddles two pages, so a page's last bytes may go unused.
const PAGE_BYTES: usize = 64 << 20;
/// Bytes read back at a time: the size of the staging buffer in system memory while a clip
/// is saved.
const CHUNK_BYTES: usize = 32 << 20;
/// How long a read-back waits between asking whether its copy has finished.
const POLL: Duration = Duration::from_millis(1);
/// Bytes in a mebibyte, for the log.
const MEBIBYTE: usize = 1 << 20;

/// Why the buffer could not be placed in GPU memory.
#[derive(Debug, Error)]
pub(crate) enum VramError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("read the adapter's memory size: {0}")]
    Adapter(windows::core::Error),
    /// The buffer is larger than the GPU's memory altogether.
    #[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
    TooLarge { needed: usize, available: usize },
    #[error("reserve a page of GPU memory: {0}")]
    Page(windows::core::Error),
}

/// The buffer's pages in GPU memory, and the device they are on.
pub(crate) struct VideoMemory {
    device: Device,
    /// Pages with packets in them, oldest first; the newest is the one being filled.
    used: VecDeque<Arc<VramPage>>,
    /// Pages with no packets in them, ready to be filled again.
    free: Vec<Arc<VramPage>>,
    /// Bytes of the newest page in use.
    fill: usize,
    /// Pages reserved up front.
    reserved: usize,
}

impl VideoMemory {
    /// Reserves pages for `bytes` of packets, and one more for the group of pictures the
    /// buffer keeps beyond its span, on a device of its own on the NVIDIA adapter. More pages
    /// are made as needed if the packets outgrow the estimate.
    pub(crate) fn create(bytes: u64) -> Result<Self, VramError> {
        let device = Device::create()?;
        let needed = usize::try_from(bytes)
            .unwrap_or(usize::MAX)
            .div_ceil(PAGE_BYTES)
            .saturating_add(1)
            .saturating_mul(PAGE_BYTES);
        let available = device
            .dedicated_video_memory()
            .map_err(VramError::Adapter)?;
        if needed > available {
            return Err(VramError::TooLarge { needed, available });
        }
        let free = (0..needed / PAGE_BYTES)
            .map(|_| VramPage::create(&device).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            device,
            used: VecDeque::new(),
            reserved: free.len(),
            free,
            fill: 0,
        })
    }

    /// What was reserved, for the log.
    pub(crate) fn description(&self) -> String {
        format!(
            "Replay buffer in GPU memory: {} MiB reserved in {} MiB pages on {}",
            self.reserved * PAGE_BYTES / MEBIBYTE,
            PAGE_BYTES / MEBIBYTE,
            self.device.adapter
        )
    }

    /// Makes a free page the one being filled, reusing one when there is one.
    fn open_page(&mut self) -> Result<(), StoreError> {
        let page = match self.free.pop() {
            Some(page) => page,
            None => Arc::new(
                VramPage::create(&self.device).map_err(StoreError::call("reserve a page"))?,
            ),
        };
        self.used.push_back(page);
        self.fill = 0;
        Ok(())
    }
}

impl Store for VideoMemory {
    fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError> {
        if data.len() > PAGE_BYTES {
            return Err(StoreError::TooLarge(data.len()));
        }
        if self.used.is_empty() || self.fill + data.len() > PAGE_BYTES {
            self.open_page()?;
        }
        let page = self.used.back().expect("a page was just opened");
        page.write(self.fill, data);
        page.live.fetch_add(1, Ordering::Relaxed);
        let slot = Slot {
            page: Arc::clone(page) as Arc<dyn Page>,
            offset: self.fill,
            length: data.len(),
        };
        self.fill += data.len();
        Ok(slot)
    }

    fn release(&mut self, slot: Slot) {
        // Packets drop oldest first, so the page is at or near the front.
        let Some(position) = self
            .used
            .iter()
            .position(|page| ptr::addr_eq(Arc::as_ptr(page), Arc::as_ptr(&slot.page)))
        else {
            return;
        };
        drop(slot);
        let page = &self.used[position];
        // The page being filled stays in use even when empty; its earlier bytes are spent.
        if page.live.fetch_sub(1, Ordering::Relaxed) != 1 || position + 1 == self.used.len() {
            return;
        }
        let page = self
            .used
            .remove(position)
            .expect("the position was just found");
        // A save still reading the page keeps it alive through its slots; it then goes away
        // with the save, and a fresh page is made when one is needed.
        if Arc::strong_count(&page) == 1 {
            self.free.push(page);
        }
    }
}

/// One page: a buffer in GPU memory on the store's device.
struct VramPage {
    buffer: ID3D11Buffer,
    d3d: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Packets in the page that the buffer has not dropped.
    live: AtomicUsize,
}

impl VramPage {
    /// A page on `device`.
    fn create(device: &Device) -> Result<Self, VramError> {
        let description = D3D11_BUFFER_DESC {
            ByteWidth: width(PAGE_BYTES),
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0.cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let buffer = create_buffer(&device.d3d, &description).map_err(VramError::Page)?;
        Ok(Self {
            buffer,
            d3d: device.d3d.clone(),
            context: device.context.clone(),
            live: AtomicUsize::new(0),
        })
    }

    /// Uploads `data` to the page at `offset`; the range lies within the page.
    fn write(&self, offset: usize, data: &[u8]) {
        let region = byte_range(offset, data.len());
        // SAFETY: the destination range lies within the live buffer and the source bytes
        // outlive the call, which copies them.
        unsafe {
            self.context.UpdateSubresource(
                &self.buffer,
                0,
                Some(&raw const region),
                data.as_ptr().cast(),
                0,
                0,
            );
        }
    }
}

impl Page for VramPage {
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        let chunk = length.min(CHUNK_BYTES);
        if chunk == 0 {
            return Ok(());
        }
        let description = D3D11_BUFFER_DESC {
            ByteWidth: width(chunk),
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let staging = create_buffer(&self.d3d, &description)
            .map_err(StoreError::call("make a staging buffer"))?;
        let mut done = 0;
        while done < length {
            let piece = chunk.min(length - done);
            let region = byte_range(offset + done, piece);
            // SAFETY: both buffers are live, and the source range lies within the page and the
            // destination within the staging buffer.
            unsafe {
                self.context.CopySubresourceRegion(
                    &staging,
                    0,
                    0,
                    0,
                    0,
                    &self.buffer,
                    0,
                    Some(&raw const region),
                );
                self.context.Flush();
            }
            let mapped = map_when_ready(&self.context, &staging)?;
            // SAFETY: the mapping is readable for `piece` bytes until it is unmapped below.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), piece) };
            sink(done, bytes);
            // SAFETY: unmaps the mapping made just above.
            unsafe { self.context.Unmap(&staging, 0) };
            done += piece;
        }
        Ok(())
    }
}

/// Maps `staging` for reading once the copy into it has finished, asking rather than waiting
/// inside the call, which would hold the device's lock against the capture thread's uploads.
fn map_when_ready(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Buffer,
) -> Result<D3D11_MAPPED_SUBRESOURCE, StoreError> {
    loop {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a live staging buffer with CPU read access; the out-parameter outlives the
        // call.
        let result = unsafe {
            context.Map(
                staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0.cast_unsigned(),
                Some(&raw mut mapped),
            )
        };
        match result {
            Ok(()) => return Ok(mapped),
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => thread::sleep(POLL),
            Err(error) => return Err(StoreError::call("read a page back")(error)),
        }
    }
}

/// A buffer on `device` as `description` says, without initial data.
fn create_buffer(
    device: &ID3D11Device,
    description: &D3D11_BUFFER_DESC,
) -> windows::core::Result<ID3D11Buffer> {
    let mut buffer = None;
    // SAFETY: a plain buffer description without initial data; the out-parameter outlives the
    // call.
    unsafe { device.CreateBuffer(description, None, Some(&raw mut buffer)) }?;
    buffer.ok_or_else(windows::core::Error::empty)
}

/// The box that selects `length` bytes at `offset` of a buffer.
fn byte_range(offset: usize, length: usize) -> D3D11_BOX {
    D3D11_BOX {
        left: width(offset),
        top: 0,
        front: 0,
        right: width(offset + length),
        bottom: 1,
        back: 1,
    }
}

/// `bytes` as Direct3D counts a buffer's bytes; a page and everything in it fit.
fn width(bytes: usize) -> u32 {
    u32::try_from(bytes).expect("a page's bytes fit in 32 bits")
}
