//! Capture and encoding on the GPU. Only compressed packets reach system memory.

mod capture;
#[expect(unsafe_code, reason = "binds the CUDA driver API")]
mod cuda;
mod cuda_output;
mod device;
#[expect(unsafe_code, reason = "loads the NVIDIA driver's libraries")]
mod ffi;
mod nvenc;
mod pacer;
mod pipeline;
mod priority;
mod vram;
mod win32;

pub(crate) use pipeline::record;

use crate::config::Codec;
use thiserror::Error;

/// The reason the NVIDIA GPU's encoder could not be queried for the codecs it supports.
#[derive(Debug, Error)]
#[error(transparent)]
pub(crate) struct ProbeError(#[from] ProbeFailure);

#[derive(Debug, Error)]
enum ProbeFailure {
    #[error(transparent)]
    Device(device::DeviceError),
    #[error(transparent)]
    Nvenc(nvenc::NvencError),
}

/// The NVIDIA GPU, as a bug report identifies it.
#[derive(Clone, Debug)]
pub(crate) struct Adapter {
    pub(crate) name: String,
    /// The driver version as NVIDIA numbers it, such as `581.57`, or `None` when the driver does
    /// not report one.
    pub(crate) driver: Option<String>,
}

/// Returns the NVIDIA GPU's name and driver version, determined without creating a device.
pub(crate) fn adapter() -> Result<Adapter, ProbeError> {
    let (name, driver) = device::describe_nvidia_adapter().map_err(ProbeFailure::Device)?;
    Ok(Adapter { name, driver })
}

/// Returns the codecs the NVIDIA GPU's encoder supports.
pub(crate) fn supported_codecs() -> Result<Vec<Codec>, ProbeError> {
    let device = device::Device::create().map_err(ProbeFailure::Device)?;
    Ok(nvenc::supported_codecs(&device.d3d).map_err(ProbeFailure::Nvenc)?)
}
