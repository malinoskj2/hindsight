//! Capture and encoding on the GPU. Frames go from Windows Graphics Capture, DXGI Desktop
//! Duplication or NvFBC into textures the encoder owns, or into CUDA memory NVENC reads in
//! place, and from there straight to NVENC; only compressed packets reach system memory.
//!
//! The submodules marked below are the only ones that call into native code directly.

#[expect(unsafe_code)]
mod capture;
#[expect(unsafe_code)]
mod cuda;
mod cuda_output;
#[expect(unsafe_code)]
mod device;
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

// The page and read-back sizes the two replay-buffer stores in GPU memory (`vram`,
// `cuda_output`) share with the one on disk.
use crate::replay::{CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, reserved_pages};
