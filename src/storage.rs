//! Application state on disk in `%LOCALAPPDATA%\Hindsight`, locked against a second instance.

use crate::config::Config;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

const MAX_STATE_BYTES: usize = 64 * 1024;
const STATE_DIRECTORY_ENV: &str = "LOCALAPPDATA";
const STATE_DIRECTORY: &str = "Hindsight";
const INSTANCE_LOCK_FILE: &str = "instance.lock";
const CONFIG_FILE: &str = "config.json";
const TOTALS_FILE: &str = "totals.json";
const BUFFER_FILE: &str = "buffer.bin";
const PENDING_EXTENSION: &str = "pending";

/// Lifetime totals, kept across runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Totals {
    pub(crate) buffered_bytes: u64,
    #[serde(alias = "written_bytes")]
    pub(crate) saved_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    _lock: File,
}

impl Store {
    pub(crate) fn open() -> Result<Self, StorageError> {
        if std::env::var_os(STATE_DIRECTORY_ENV).is_none() {
            return Err(StorageError::NoDirectory);
        }
        Self::at(state_directory())
    }

    pub(crate) fn at(root: PathBuf) -> Result<Self, StorageError> {
        fs::create_dir_all(&root)?;
        let lock = lock_instance(&root)?;
        Ok(Self { root, _lock: lock })
    }

    pub(crate) fn load_config(&self) -> Result<Config, StorageError> {
        Ok(self.read(CONFIG_FILE)?.unwrap_or_default())
    }

    pub(crate) fn save_config(&self, config: &Config) -> Result<(), StorageError> {
        self.write(CONFIG_FILE, config)
    }

    pub(crate) fn load_totals(&self) -> Result<Totals, StorageError> {
        Ok(self.read(TOTALS_FILE)?.unwrap_or_default())
    }

    pub(crate) fn save_totals(&self, totals: &Totals) -> Result<(), StorageError> {
        self.write(TOTALS_FILE, totals)
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, StorageError> {
        let Some(file) = open_if_present(&self.root.join(name))? else {
            return Ok(None);
        };
        let bytes = read_capped(file)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), StorageError> {
        let bytes = within_cap(serde_json::to_vec_pretty(value)?)?;
        write_atomically(&self.root.join(name), &bytes)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("the application state could not be read or written")]
    Io(#[from] io::Error),
    #[error("the application state is not valid JSON")]
    Json(#[from] serde_json::Error),
    #[error("application state is larger than {} KB", MAX_STATE_BYTES / 1024)]
    Oversized,
    #[error("another hindsight instance is running")]
    Locked,
    #[error("{} is unavailable", STATE_DIRECTORY_ENV)]
    NoDirectory,
}

/// Without `LOCALAPPDATA` paths fall back to the temporary directory; [`Store::open`] refuses.
pub(crate) fn state_directory() -> PathBuf {
    std::env::var_os(STATE_DIRECTORY_ENV)
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(STATE_DIRECTORY)
}

pub(crate) fn buffer_file() -> PathBuf {
    state_directory().join(BUFFER_FILE)
}

fn lock_instance(root: &Path) -> Result<File, StorageError> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(INSTANCE_LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StorageError::Locked),
        Err(TryLockError::Error(error)) => Err(StorageError::Io(error)),
    }
}

fn open_if_present(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_capped(file: File) -> Result<Vec<u8>, StorageError> {
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    within_cap(bytes)
}

fn within_cap(bytes: Vec<u8>) -> Result<Vec<u8>, StorageError> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(StorageError::Oversized);
    }
    Ok(bytes)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(PENDING_EXTENSION);
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    // Windows cannot rename a file that is still open.
    drop(file);

    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn totals_survive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        assert_eq!(store.load_totals().unwrap(), Totals::default());

        store
            .save_totals(&Totals {
                buffered_bytes: 9_000_000_000,
                saved_bytes: 3_000_000_000,
            })
            .unwrap();
        drop(store);

        assert_eq!(
            Store::at(directory.path().to_owned())
                .unwrap()
                .load_totals()
                .unwrap(),
            Totals {
                buffered_bytes: 9_000_000_000,
                saved_bytes: 3_000_000_000,
            }
        );
    }

    #[test]
    fn configuration_round_trips_and_lock_excludes_second_instance() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();

        assert_matches!(
            Store::at(directory.path().to_owned()),
            Err(StorageError::Locked)
        );
        assert_eq!(store.load_config().unwrap(), Config::default());

        let config = Config {
            buffer_seconds: 90,
            ..Config::default()
        };
        store.save_config(&config).unwrap();

        drop(store);
        assert_eq!(
            Store::at(directory.path().to_owned())
                .unwrap()
                .load_config()
                .unwrap(),
            config
        );
    }

    #[test]
    fn corrupt_state_is_not_silently_reset() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        fs::write(directory.path().join(CONFIG_FILE), b"broken").unwrap();
        assert_matches!(store.load_config(), Err(StorageError::Json(_)));
    }

    #[test]
    fn the_cause_is_printed_once_through_the_chain() {
        let error = StorageError::from(io::Error::other("access is denied"));
        assert_eq!(
            crate::log::chain(&error).to_string(),
            "the application state could not be read or written: access is denied"
        );
    }

    #[test]
    fn oversized_state_is_rejected_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();

        assert_matches!(
            store.write("large.json", &"x".repeat(65_537)),
            Err(StorageError::Oversized)
        );
        assert!(!directory.path().join("large.json").exists());
    }
}
