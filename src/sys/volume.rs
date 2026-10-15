//! The free space of a drive.

use std::{io, path::Path};

/// Returns the free bytes available to this user (quotas apply) on the drive that holds
/// `directory`, which must exist.
pub(crate) fn free_bytes(directory: &Path) -> io::Result<u64> {
    let mut free = 0;
    winsafe::GetDiskFreeSpaceEx(
        Some(&directory.to_string_lossy()),
        Some(&mut free),
        None,
        None,
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw().cast_signed()))?;
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
