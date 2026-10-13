//! Capture and encoding on the GPU; only compressed packets reach system memory.

#[expect(unsafe_code)]
mod capture;
#[expect(unsafe_code)]
mod cuda;
mod cuda_output;
#[expect(unsafe_code)]
mod device;
#[expect(unsafe_code)]
mod ffi;
#[expect(unsafe_code)]
mod nvenc;
#[expect(unsafe_code)]
mod pacer;
mod pipeline;
#[expect(unsafe_code)]
mod priority;
#[expect(unsafe_code)]
mod vram;

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
