//! The memory installed in the machine, which the default buffer place depends on.

use windows::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory;

const BYTES_PER_KILOBYTE: u64 = 1024;

/// Returns the system memory installed, in bytes. Unlike the memory Windows can use, it counts
/// what the firmware and an integrated GPU reserve, so 32 GB of memory reads as 32 GiB.
pub(crate) fn installed() -> windows::core::Result<u64> {
    let mut kilobytes = 0;
    // SAFETY: the out-parameter outlives the call.
    unsafe { GetPhysicallyInstalledSystemMemory(&raw mut kilobytes) }?;
    Ok(kilobytes.saturating_mul(BYTES_PER_KILOBYTE))
}
