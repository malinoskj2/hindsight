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

// What the two replay-buffer stores in GPU memory (`vram`, `cuda_output`) share.

/// Bytes in a page of a replay buffer in GPU memory. A packet never straddles two pages, so a
/// page's last bytes may go unused.
const PAGE_BYTES: usize = 64 << 20;
/// Bytes a save reads back at a time: the size of the buffer in system memory it reads
/// through.
const CHUNK_BYTES: usize = 32 << 20;
/// Bytes in a mebibyte, for the log.
const MEBIBYTE: usize = 1 << 20;

/// Pages to reserve up front for `bytes` of packets: enough to hold them, and one more for the
/// group of pictures the buffer keeps beyond its span.
fn reserved_pages(bytes: u64) -> usize {
    usize::try_from(bytes)
        .unwrap_or(usize::MAX)
        .div_ceil(PAGE_BYTES)
        .saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_reserves_its_pages_and_one_more() {
        let page = PAGE_BYTES as u64;
        assert_eq!(reserved_pages(0), 1);
        assert_eq!(reserved_pages(1), 2);
        assert_eq!(reserved_pages(page), 2);
        assert_eq!(reserved_pages(page + 1), 3);
        assert_eq!(reserved_pages(10 * page), 11);
    }
}
