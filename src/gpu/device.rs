//! The Direct3D 11 device capture and NVENC share, created on the NVIDIA adapter.

use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::{
                CreateDXGIFactory1, DXGI_ADAPTER_DESC, DXGI_ADAPTER_DESC1,
                DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1, IDXGIDevice,
                IDXGIFactory1,
            },
        },
    },
    core::Interface,
};

const NVIDIA_VENDOR_ID: u32 = 0x10DE;
/// Windows grants more than 0 only to an elevated process.
const GPU_THREAD_PRIORITY: i32 = 7;

#[derive(Debug, Error)]
pub(crate) enum DeviceError {
    #[error("no NVIDIA GPU was found; NVENC needs one")]
    NoNvidiaAdapter,
    #[error("list the graphics adapters: {0}")]
    Enumerate(windows::core::Error),
    #[error("create the Direct3D 11 device on {0}: {1}")]
    Create(String, windows::core::Error),
}

pub(super) struct Device {
    pub(super) d3d: ID3D11Device,
    pub(super) context: ID3D11DeviceContext,
    pub(super) adapter: String,
}

impl Device {
    pub(super) fn create() -> Result<Self, DeviceError> {
        let (adapter, name) = nvidia_adapter()?;
        let created = |error| DeviceError::Create(name.clone(), error);
        let (d3d, context) = create_device(&adapter).map_err(created)?;
        // The capture frame pool works on the device from its own threads.
        protect_multithreaded(&context).map_err(created)?;

        Ok(Self {
            d3d,
            context,
            adapter: name,
        })
    }

    pub(super) fn copy(&self, target: &ID3D11Texture2D, source: &ID3D11Texture2D) {
        // SAFETY: two live textures of the same shape on this device.
        unsafe { self.context.CopyResource(target, source) };
    }

    pub(super) fn copy_region(
        &self,
        target: &ID3D11Texture2D,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) {
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both textures are of one format on this device and the region lies within each.
        unsafe {
            self.context.CopySubresourceRegion(
                target,
                0,
                0,
                0,
                0,
                source,
                0,
                Some(&raw const region),
            );
        }
    }

    pub(super) fn dxgi(&self) -> windows::core::Result<IDXGIDevice> {
        self.d3d.cast()
    }

    pub(super) fn luid(&self) -> windows::core::Result<LUID> {
        Ok(self.adapter_description()?.AdapterLuid)
    }

    pub(super) fn dedicated_video_memory(&self) -> windows::core::Result<usize> {
        Ok(self.adapter_description()?.DedicatedVideoMemory)
    }

    fn adapter_description(&self) -> windows::core::Result<DXGI_ADAPTER_DESC> {
        let device = self.dxgi()?;
        // SAFETY: reads the adapter of a live device and then its description.
        unsafe { device.GetAdapter()?.GetDesc() }
    }

    pub(super) fn raise_priority(&self) -> windows::core::Result<()> {
        let device = self.dxgi()?;
        // SAFETY: a plain setter on a live device.
        unsafe { device.SetGPUThreadPriority(GPU_THREAD_PRIORITY) }
    }
}

fn nvidia_adapter() -> Result<(IDXGIAdapter1, String), DeviceError> {
    // SAFETY: creates a factory; no other preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(DeviceError::Enumerate)?;
    for index in 0.. {
        // SAFETY: enumeration ends with DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(DeviceError::Enumerate(error)),
        };
        // SAFETY: reads the description of a live adapter.
        let description = unsafe { adapter.GetDesc1() }.map_err(DeviceError::Enumerate)?;
        if is_nvidia_hardware(&description) {
            return Ok((adapter, adapter_name(&description)));
        }
    }
    Err(DeviceError::NoNvidiaAdapter)
}

fn is_nvidia_hardware(description: &DXGI_ADAPTER_DESC1) -> bool {
    let software = description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned() != 0;
    description.VendorId == NVIDIA_VENDOR_ID && !software
}

fn adapter_name(description: &DXGI_ADAPTER_DESC1) -> String {
    let length = description
        .Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(description.Description.len());
    String::from_utf16_lossy(&description.Description[..length])
}

/// BGRA support is required by Windows Graphics Capture.
fn create_device(
    adapter: &IDXGIAdapter1,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: live adapter; the out-parameters outlive the call.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )
    }?;
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(windows::core::Error::empty()),
    }
}

fn protect_multithreaded(context: &ID3D11DeviceContext) -> windows::core::Result<()> {
    let multithread: ID3D11Multithread = context.cast()?;
    // SAFETY: switches on the device's internal locking; returns the previous setting.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    Ok(())
}
