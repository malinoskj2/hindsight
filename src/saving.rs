//! Saving a clip or a screenshot as a file named for the time.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use thiserror::Error;

mod name;

pub(crate) use name::SavedName;

const PARTIAL_EXTENSION: &str = "partial";
/// The number of files saved within one second before a save gives up looking for a free name.
const NAME_ATTEMPTS: u32 = 1000;
const RENAME_ATTEMPTS: u32 = 10;
const RENAME_RETRY: Duration = Duration::from_millis(200);
pub(crate) const WRITE_BUFFER_BYTES: usize = 1 << 20;

/// The reason a file could not be saved. The message names only the failed step and the path,
/// and the cause is its `source()`, so it is displayed with [`crate::log::chain`].
#[derive(Debug, Error)]
#[error("{step} {}", path.display())]
pub(crate) struct WriteError {
    step: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
}

impl WriteError {
    pub(crate) fn at(step: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        move |source| Self {
            step,
            path: path.to_owned(),
            source,
        }
    }
}

/// The file is written under a `.partial` name and renamed to its final one only once complete.
/// A complete file that cannot be renamed is kept under the `.partial` name, which the error
/// gives, rather than lost.
pub(crate) fn save_file<T, E: From<WriteError>>(
    directory: &Path,
    extension: &str,
    write: impl FnOnce(&Path) -> Result<T, E>,
) -> Result<(PathBuf, T), E> {
    fs::create_dir_all(directory).map_err(WriteError::at("create the folder", directory))?;
    let name = SavedName::now();
    let partial = directory
        .join(name.file_name(extension))
        .with_extension(PARTIAL_EXTENSION);
    let written = write(&partial).inspect_err(|_| {
        let _ = fs::remove_file(&partial);
    })?;
    let path = claim_name(directory, name, extension, &partial)
        .map_err(WriteError::at("name the finished file", &partial))?;
    Ok((path, written))
}

/// Moves the finished `partial` to the first attempt of `name` that no file has. Each name is
/// claimed by creating it, so a file that appears in the meantime is never replaced.
fn claim_name(
    directory: &Path,
    name: SavedName,
    extension: &str,
    partial: &Path,
) -> io::Result<PathBuf> {
    let mut name = name;
    for _ in 0..NAME_ATTEMPTS {
        let path = directory.join(name.file_name(extension));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(claimed) => {
                drop(claimed);
                return match rename(partial, &path) {
                    Ok(()) => Ok(path),
                    Err(error) => {
                        let _ = fs::remove_file(&path);
                        Err(error)
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => name = name.next(),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every name for this second is taken",
    ))
}

/// Renames `from` over the name just claimed, trying again for a while: an antivirus scanner,
/// a sync client or Explorer can briefly hold the claimed file open after it appears.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempts = 1;
    loop {
        match fs::rename(from, to) {
            Err(_) if attempts < RENAME_ATTEMPTS => {
                attempts += 1;
                thread::sleep(RENAME_RETRY);
            }
            renamed => return renamed,
        }
    }
}

pub(crate) fn write_file<E: From<WriteError>>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), E>,
) -> Result<(), E> {
    let file = File::create(path).map_err(WriteError::at("create", path))?;
    let mut output = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    write(&mut output)?;
    sync(output).map_err(WriteError::at("finish", path))?;
    Ok(())
}

fn sync(output: BufWriter<File>) -> io::Result<()> {
    let file = output
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log;

    #[test]
    fn a_write_error_names_its_step_and_chains_its_cause_once() {
        let error = WriteError::at("create", Path::new("clip.mp4"))(io::Error::other("denied"));
        assert_eq!(error.to_string(), "create clip.mp4");
        assert_eq!(log::chain(&error).to_string(), "create clip.mp4: denied");
    }

    #[test]
    fn a_name_another_file_holds_is_skipped_not_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let name = SavedName::parse("hindsight_2026-10-12_21-14-05.mp4").unwrap();
        let taken = directory.path().join(name.file_name("mp4"));
        fs::write(&taken, b"theirs").unwrap();
        let partial = directory.path().join("clip.partial");
        fs::write(&partial, b"ours").unwrap();

        let path = claim_name(directory.path(), name, "mp4", &partial).unwrap();

        assert_eq!(
            path,
            directory.path().join("hindsight_2026-10-12_21-14-05-2.mp4")
        );
        assert_eq!(fs::read(&path).unwrap(), b"ours");
        assert_eq!(fs::read(&taken).unwrap(), b"theirs");
        assert!(!partial.exists());
    }

    #[test]
    fn a_file_is_complete_under_its_name_or_absent() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("saved");

        let (path, ()) = save_file(&folder, "txt", |partial| {
            write_file(partial, |output| {
                io::Write::write_all(output, b"complete").map_err(WriteError::at("write", partial))
            })
        })
        .unwrap();
        assert_eq!(path.extension().unwrap(), "txt");
        assert_eq!(fs::read(&path).unwrap(), b"complete");

        let refused: Result<(PathBuf, ()), WriteError> = save_file(&folder, "txt", |partial| {
            Err(WriteError::at("write", partial)(io::Error::other("full")))
        });
        assert_eq!(
            refused.unwrap_err().to_string().split(' ').next(),
            Some("write")
        );
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_complete_file_that_cannot_be_renamed_is_kept() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;

        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("saved");

        // The handle, shared only for reading, is returned so that it holds the file open while
        // the save renames it.
        let refused = save_file(&folder, "txt", |partial| {
            fs::write(partial, b"complete").map_err(WriteError::at("write", partial))?;
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(partial)
                .map_err(WriteError::at("hold", partial))
        });

        let error = refused.unwrap_err();
        assert!(error.to_string().starts_with("name the finished file"));
        let kept: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].extension().unwrap(), PARTIAL_EXTENSION);
        assert_eq!(fs::read(&kept[0]).unwrap(), b"complete");
    }
}
