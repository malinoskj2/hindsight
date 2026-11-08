//! Puts a file on the clipboard the way Explorer's Copy does, so that it pastes into chat apps
//! and folders.

use bytemuck::{Pod, Zeroable};
use std::{os::windows::ffi::OsStrExt, path::Path, thread, time::Duration};
use thiserror::Error;
use winsafe::{self as w, co, guard::CloseClipboardGuard, prelude::*};

/// Another program may hold the clipboard open for a moment, as clipboard managers do after
/// every change.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY: Duration = Duration::from_millis(20);

#[derive(Debug, Error)]
pub(crate) enum ClipboardError {
    #[error("the clipboard is held open by another program")]
    Open(#[source] co::ERROR),
    #[error("the clipboard could not be emptied")]
    Empty(#[source] co::ERROR),
    #[error("the file could not be put on the clipboard")]
    Set(#[source] co::ERROR),
}

/// Win32's `DROPFILES`, which heads the `CF_HDROP` format.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DropFiles {
    /// The offset of the path list from the start of this header.
    files: u32,
    point: [i32; 2],
    non_client: i32,
    wide: i32,
}

/// Replaces what the clipboard holds with the file at `path`, given as a full path.
pub(crate) fn copy_file(path: &Path) -> Result<(), ClipboardError> {
    let clipboard = open()?;
    clipboard.EmptyClipboard().map_err(ClipboardError::Empty)?;
    clipboard
        .SetClipboardData(co::CF::HDROP, &file_list(path))
        .map_err(ClipboardError::Set)
}

fn open() -> Result<CloseClipboardGuard, ClipboardError> {
    for _ in 1..OPEN_ATTEMPTS {
        if let Ok(clipboard) = w::HWND::NULL.OpenClipboard() {
            return Ok(clipboard);
        }
        thread::sleep(OPEN_RETRY);
    }
    w::HWND::NULL.OpenClipboard().map_err(ClipboardError::Open)
}

/// The `CF_HDROP` data for one file: the header, then the path in UTF-16 ended by two nulls.
fn file_list(path: &Path) -> Vec<u8> {
    let header = DropFiles {
        files: super::win32::cb_size::<DropFiles>(),
        wide: 1,
        ..DropFiles::zeroed()
    };
    let path = path.as_os_str().encode_wide().chain([0, 0]);
    bytemuck::bytes_of(&header)
        .iter()
        .copied()
        .chain(path.flat_map(u16::to_le_bytes))
        .collect()
}

/// Changes whenever any application changes the clipboard's contents.
pub(crate) fn revision() -> u32 {
    w::HCLIPBOARD::NULL.GetClipboardSequenceNumber()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::DROPFILES;

    #[test]
    fn the_header_matches_win32s_and_the_path_follows_it_ended_by_two_nulls() {
        assert_eq!(size_of::<DropFiles>(), size_of::<DROPFILES>());
        let list = file_list(Path::new(r"D:\clips\é.mp4"));
        let (header, path) = list.split_at(size_of::<DropFiles>());
        assert_eq!(header[..4], 20u32.to_le_bytes());
        assert_eq!(header[16..], 1u32.to_le_bytes());
        let units: Vec<u16> = path
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let (text, end) = units.split_at(units.len() - 2);
        assert_eq!(String::from_utf16(text).unwrap(), r"D:\clips\é.mp4");
        assert_eq!(end, [0, 0]);
    }

    #[test]
    #[ignore = "replaces what the clipboard holds"]
    fn by_hand_a_copied_file_is_on_the_clipboard_as_a_file_list() {
        let file = tempfile::NamedTempFile::new().unwrap();
        copy_file(file.path()).unwrap();
        let clipboard = w::HWND::NULL.OpenClipboard().unwrap();
        let data = clipboard.GetClipboardData(co::CF::HDROP).unwrap();
        assert_eq!(data[..file_list(file.path()).len()], file_list(file.path()));
    }
}
