//! Application state on disk, in `%LOCALAPPDATA%\Hindsight`.
//!
//! Opening the store takes an exclusive lock on `instance.lock` there and holds it until the
//! store is dropped, so a second copy of hindsight fails to start instead of two recorders saving
//! over each other's settings. Each state file is capped at 64 KB, so a damaged file is
//! reported rather than read without bound. Writes go to a `.pending` file that is then renamed
//! over the original, so a crash part-way through leaves the previous state intact instead of a
//! truncated file. Without `LOCALAPPDATA` the temporary directory stands in for building paths,
//! but opening the store reports the missing variable rather than keeping state somewhere that
//! may be cleared.

use crate::config::Config;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
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
const PENDING_EXTENSION: &str = "pending";

/// Lifetime totals, kept across runs. `written_bytes` is the name the total had before it was
/// split into these two, so files from that version still load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Totals {
    /// Bytes that went into the replay buffer, in every run.
    pub(crate) buffered_bytes: u64,
    /// Bytes of the clips saved to disk, in every run.
    #[serde(alias = "written_bytes")]
    pub(crate) saved_bytes: u64,
}

#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("state I/O: {0}")]
    Io(#[from] io::Error),
    #[error("invalid state JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("application state is larger than {} KB", MAX_STATE_BYTES / 1024)]
    Oversized,
    #[error("another hindsight instance is running, or the state directory cannot be locked")]
    Locked,
    #[error("{} is unavailable", STATE_DIRECTORY_ENV)]
    NoDirectory,
}

#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    /// Held only to keep the instance lock; closing the file releases it.
    _lock: File,
}

/// The application state directory (`%LOCALAPPDATA%\Hindsight`). Without `LOCALAPPDATA` the
/// temporary directory stands in, so paths can still be formed; [`Store::open`] reports the
/// missing variable as an error instead.
pub(crate) fn state_directory() -> PathBuf {
    std::env::var_os(STATE_DIRECTORY_ENV)
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(STATE_DIRECTORY)
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
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(INSTANCE_LOCK_FILE))?;
        lock.try_lock().map_err(|_| StorageError::Locked)?;

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

    pub(crate) fn save_totals(&self, totals: Totals) -> Result<(), StorageError> {
        self.write(TOTALS_FILE, &totals)
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, StorageError> {
        let file = match File::open(self.root.join(name)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(StorageError::Oversized);
        }

        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), StorageError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(StorageError::Oversized);
        }

        atomic_write(&self.root.join(name), &bytes)?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
            .save_totals(Totals {
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
