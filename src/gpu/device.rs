//! The Direct3D 11 device that capture and NVENC share. It is created on the NVIDIA adapter, so
//! NVENC reads the captured textures where they already are, even on a laptop whose display
//! hangs off the integrated GPU.

use thiserror::Error;
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::{
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
                IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
            },
        },
    },
    core::Interface,
};

const NVIDIA_VENDOR_ID: u32 = 0x10DE;
/// The highest GPU thread priority; Windows grants more than 0 only to an elevated process.
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

        let mut device = None;
        let mut context = None;
        // SAFETY: out-parameters are written on success; BGRA support is what Windows Graphics
        // Capture requires of the device.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }
        .map_err(created)?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(created(windows::core::Error::empty()));
        };

        // The capture frame pool works on the device from its own threads.
        let multithread: ID3D11Multithread = context.cast().map_err(created)?;
        // SAFETY: switches on the device's internal locking; returns the previous setting.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        Ok(Self {
            d3d: device,
            context,
            adapter: name,
        })
    }

    /// Copies `source` into `target` on the GPU; both have the same size and format.
    pub(super) fn copy(&self, target: &ID3D11Texture2D, source: &ID3D11Texture2D) {
        // SAFETY: two live textures of the same shape on this device.
        unsafe { self.context.CopyResource(target, source) };
    }

    /// Lets the device's copies run ahead of a game that keeps the GPU busy.
    pub(super) fn raise_priority(&self) -> windows::core::Result<()> {
        let device: IDXGIDevice = self.d3d.cast()?;
        // SAFETY: a plain setter on a live device.
        unsafe { device.SetGPUThreadPriority(GPU_THREAD_PRIORITY) }
    }
}

/// The first hardware adapter from NVIDIA, and its name.
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
        let software = description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned() != 0;
        if description.VendorId == NVIDIA_VENDOR_ID && !software {
            let length = description
                .Description
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(description.Description.len());
            let name = String::from_utf16_lossy(&description.Description[..length]);
            return Ok((adapter, name));
        }
    }
    Err(DeviceError::NoNvidiaAdapter)
}
