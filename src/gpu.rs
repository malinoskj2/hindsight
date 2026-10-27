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
mod still;
mod vram;

pub(crate) use pipeline::record;
pub(crate) use still::grab;

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

/// The NVIDIA GPU's room for the buffer and whether NvFBC can see a display on it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Capacity {
    pub(crate) video_memory: u64,
    pub(crate) drives_display: bool,
}

/// Returns the NVIDIA GPU's dedicated memory and whether a display is attached to it,
/// determined without creating a device.
pub(crate) fn capacity() -> Result<Capacity, ProbeError> {
    let (video_memory, drives_display) = device::nvidia_capacity().map_err(ProbeFailure::Device)?;
    Ok(Capacity {
        video_memory,
        drives_display,
    })
}

/// Returns the codecs the NVIDIA GPU's encoder supports.
pub(crate) fn supported_codecs() -> Result<Vec<Codec>, ProbeError> {
    let device = device::Device::create().map_err(ProbeFailure::Device)?;
    Ok(nvenc::supported_codecs(&device.d3d).map_err(ProbeFailure::Nvenc)?)
}
