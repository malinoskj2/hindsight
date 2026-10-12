//! The free space of a drive, which a replay buffer on disk checks before it reserves its file.

use std::{io, path::Path};
use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::HSTRING};

/// Bytes free for this user on the drive that holds `directory`, which must exist; disk quotas
/// are taken into account.
pub(crate) fn free_bytes(directory: &Path) -> io::Result<u64> {
    let mut free = 0;
    // SAFETY: the path is a NUL-terminated wide string that outlives the call, and the one
    // out-parameter asked for points at a live u64.
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
