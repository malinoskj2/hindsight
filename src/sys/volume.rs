//! The free space of a drive.

use std::{io, path::Path};
use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::HSTRING};

/// Free bytes for this user (quotas apply) on the drive holding `directory`, which must exist.
pub(crate) fn free_bytes(directory: &Path) -> io::Result<u64> {
    let mut free = 0;
    // SAFETY: the wide string outlives the call and the out-parameter is a live u64.
    unsafe {
        GetDiskFreeSpaceExW(
            &HSTRING::from(directory.as_os_str()),
            Some(&raw mut free),
            None,
            None,
        )
    }?;
    Ok(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temporary_directory_has_some_room() {
        assert!(free_bytes(&std::env::temp_dir()).unwrap() > 0);
        let missing = std::env::temp_dir()
            .join("hindsight-no-such-folder")
            .join("inside");
        assert!(free_bytes(&missing).is_err());
    }
}
