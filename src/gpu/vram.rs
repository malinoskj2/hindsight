//! The replay buffer in GPU memory, on a Direct3D device of its own so saves never queue behind capture.

use crate::{
    gpu::device::{Device, DeviceError},
    replay::{
        CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, Page, Pages, Slot, Store, StoreError, reserved_pages,
    },
};
use std::{thread, time::Duration};
use thiserror::Error;
use windows::Win32::Graphics::{
    Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
        D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_DEFAULT,
        D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    },
    Dxgi::DXGI_ERROR_WAS_STILL_DRAWING,
};

const POLL: Duration = Duration::from_millis(1);

#[derive(Debug, Error)]
pub(super) enum VramError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("read the adapter's memory size: {0}")]
    Adapter(windows::core::Error),
    #[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
    TooLarge { needed: usize, available: usize },
    #[error("reserve a page of GPU memory: {0}")]
    Page(windows::core::Error),
}

pub(super) struct VideoMemory {
    device: Device,
    pages: Pages<VramPage>,
    reserved: usize,
}

impl VideoMemory {
    pub(super) fn create(bytes: u64) -> Result<Self, VramError> {
        let device = Device::create()?;
        let pages = reserved_pages(bytes);
        let needed = pages.saturating_mul(PAGE_BYTES);
        let available = device
            .dedicated_video_memory()
            .map_err(VramError::Adapter)?;
        if needed > available {
            return Err(VramError::TooLarge { needed, available });
        }
        let reserved = (0..pages)
            .map(|_| VramPage::create(&device))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            device,
            reserved: reserved.len(),
            pages: Pages::new(PAGE_BYTES, reserved),
        })
    }

    pub(super) fn description(&self) -> String {
        format!(
            "Replay buffer in GPU memory: {} MiB reserved in {} MiB pages on {}",
            self.reserved * PAGE_BYTES / MEBIBYTE,
            PAGE_BYTES / MEBIBYTE,
            self.device.adapter
        )
    }
}

impl Store for VideoMemory {
    fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError> {
        let device = &self.device;
        let (page, offset) = self.pages.place(data.len(), || {
            VramPage::create(device).map_err(StoreError::call("reserve a page"))
        })?;
        page.write(offset, data);
        Ok(Slot::new(page, offset, data.len()))
    }
}

struct VramPage {
    buffer: ID3D11Buffer,
    d3d: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl VramPage {
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
        })
    }

    fn write(&self, offset: usize, data: &[u8]) {
        let region = byte_range(offset, data.len());
        // SAFETY: the range lies within the live buffer; the source is copied during the call.
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
            // SAFETY: both buffers are live and the ranges lie within them.
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

/// Polls with `DO_NOT_WAIT`: a waiting `Map` would hold the device lock against the capture thread's uploads.
fn map_when_ready(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Buffer,
) -> Result<D3D11_MAPPED_SUBRESOURCE, StoreError> {
    loop {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: live staging buffer with CPU read access.
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

fn create_buffer(
    device: &ID3D11Device,
    description: &D3D11_BUFFER_DESC,
) -> windows::core::Result<ID3D11Buffer> {
    let mut buffer = None;
    // SAFETY: a plain buffer description without initial data.
    unsafe { device.CreateBuffer(description, None, Some(&raw mut buffer)) }?;
    buffer.ok_or_else(windows::core::Error::empty)
}

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

fn width(bytes: usize) -> u32 {
    u32::try_from(bytes).expect("a page's bytes fit in 32 bits")
}
