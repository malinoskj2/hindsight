//! Capture and encoding on the GPU; only compressed packets reach system memory.

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

/// Why the NVIDIA GPU's encoder could not be asked which codecs it supports.
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

/// The codecs the NVIDIA GPU's encoder supports.
pub(crate) fn supported_codecs() -> Result<Vec<Codec>, ProbeError> {
    let device = device::Device::create().map_err(ProbeFailure::Device)?;
    Ok(nvenc::supported_codecs(&device.d3d).map_err(ProbeFailure::Nvenc)?)
}
