//! Capture and encoding on the GPU. Frames go from Windows Graphics Capture into textures the
//! encoder owns and from there straight to NVENC; only compressed packets reach system memory.
//!
//! The submodules marked below are the only ones that call into native code directly.

#[expect(unsafe_code)]
mod capture;
#[expect(unsafe_code)]
mod device;
#[expect(unsafe_code)]
mod nvenc;
#[expect(unsafe_code)]
mod pacer;
mod pipeline;
#[expect(unsafe_code)]
mod priority;

pub(crate) use pipeline::record;
