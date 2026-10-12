//! Capture and encoding on the GPU; only compressed packets reach system memory.

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

use crate::replay::{CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, reserved_pages};
