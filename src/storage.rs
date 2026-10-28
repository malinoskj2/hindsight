//! Application state on disk in `%LOCALAPPDATA%\Hindsight`, locked against a second instance.

use crate::{config::Config, update::Version};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

const MAX_STATE_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const STATE_DIRECTORY: &str = "Hindsight";
const INSTANCE_LOCK_FILE: &str = "instance.lock";
const CONFIG_FILE: &str = "config.json";
const TOTALS_FILE: &str = "totals.json";
const LAST_RUN_FILE: &str = "last_run.json";
#[cfg(windows)]
const BUFFER_FILE: &str = "buffer.bin";
const PENDING_EXTENSION: &str = "pending";
const FIRST_RELEASE: &str = "0.1.0";
/// The settings now suited to the machine, with the values 0.1.0 wrote for them by default.
const FIRST_RELEASE_DEFAULTS: [(&str, &str); 2] = [("backend", "nvfbc"), ("buffer_memory", "ram")];

/// Lifetime totals, kept across runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Totals {
    pub(crate) buffered_bytes: u64,
    #[serde(alias = "written_bytes")]
    pub(crate) saved_bytes: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct LastRun {
    version: Option<Version>,
}

#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    _lock: File,
}

impl Store {
    #[cfg(windows)]
    pub(crate) fn open() -> Result<Self, StorageError> {
        let base = local_app_data().ok_or(StorageError::NoDirectory)?;
        Self::at(base.join(STATE_DIRECTORY))
    }

    pub(crate) fn at(root: PathBuf) -> Result<Self, StorageError> {
        fs::create_dir_all(&root)?;
        let lock = lock_instance(&root)?;
        Ok(Self { root, _lock: lock })
    }

    pub(crate) fn load_config(&self, defaults: &Config) -> Result<Config, StorageError> {
        let mut settings = settings(defaults)?;
        settings.extend(self.chosen_settings()?);
        Ok(serde_json::from_value(Value::Object(settings))?)
    }

    /// A setting the user never changed is left out, so it keeps following the machine's defaults.
    pub(crate) fn save_config(
        &self,
        previous: &Config,
        config: &Config,
    ) -> Result<(), StorageError> {
        let previous = settings(previous)?;
        let current = settings(config)?;
        let mut chosen = self.chosen_settings()?;
        chosen.retain(|name, _| current.contains_key(name));
        for (name, value) in current {
            if previous.get(&name) != Some(&value) {
                chosen.insert(name, value);
            }
        }
        self.write(CONFIG_FILE, &chosen)
    }

    fn chosen_settings(&self) -> Result<Map<String, Value>, StorageError> {
        Ok(self.read(CONFIG_FILE)?.unwrap_or_default())
    }

    pub(crate) fn load_totals(&self) -> Result<Totals, StorageError> {
        Ok(self.read(TOTALS_FILE)?.unwrap_or_default())
    }

    pub(crate) fn save_totals(&self, totals: &Totals) -> Result<(), StorageError> {
        self.write(TOTALS_FILE, totals)
    }

    /// Records state that 0.1.0 left as 0.1.0's, since it kept no record of the version that ran.
    /// 0.1.0 also wrote every setting, chosen or not, so the settings now suited to the machine
    /// are left to the machine where they hold 0.1.0's defaults. Returns whether there was such
    /// state.
    pub(crate) fn adopt_first_release(&self) -> Result<bool, StorageError> {
        let exists = |name| self.root.join(name).try_exists();
        if exists(LAST_RUN_FILE)? || !(exists(CONFIG_FILE)? || exists(TOTALS_FILE)?) {
            return Ok(false);
        }
        if let Some(mut chosen) = self.read::<Map<String, Value>>(CONFIG_FILE)? {
            for (name, default) in FIRST_RELEASE_DEFAULTS {
                if chosen.get(name).and_then(Value::as_str) == Some(default) {
                    chosen.remove(name);
                }
            }
            self.write(CONFIG_FILE, &chosen)?;
        }
        let first = FIRST_RELEASE
            .parse()
            .expect("the first release's version is a semantic version");
        self.save_last_version(&first)?;
        Ok(true)
    }

    pub(crate) fn load_last_version(&self) -> Result<Option<Version>, StorageError> {
        Ok(self
            .read::<LastRun>(LAST_RUN_FILE)?
            .and_then(|last| last.version))
    }

    pub(crate) fn save_last_version(&self, version: &Version) -> Result<(), StorageError> {
        self.write(
            LAST_RUN_FILE,
            &LastRun {
                version: Some(version.clone()),
            },
        )
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
    #[error("the application state is invalid")]
    Json(#[from] serde_json::Error),
    #[error("application state is larger than {} KB", MAX_STATE_BYTES / 1024)]
    Oversized,
    #[error("another hindsight instance is running")]
    Locked,
    #[cfg(windows)]
    #[error("the local application data folder is unavailable")]
    NoDirectory,
}

/// Returns the state directory. Without the local application data folder, paths fall back to
/// the temporary directory, while [`Store::open`] returns an error.
#[cfg(windows)]
pub(crate) fn state_directory() -> PathBuf {
    local_app_data()
        .unwrap_or_else(std::env::temp_dir)
        .join(STATE_DIRECTORY)
}

/// Returns `%LOCALAPPDATA%` from the shell rather than the environment, which can be stale.
#[cfg(windows)]
fn local_app_data() -> Option<PathBuf> {
    crate::sys::com::known_folder(&winsafe::co::KNOWNFOLDERID::LocalAppData)
}

#[cfg(windows)]
pub(crate) fn buffer_file() -> PathBuf {
    state_directory().join(BUFFER_FILE)
}

fn settings(config: &Config) -> Result<Map<String, Value>, StorageError> {
    Ok(serde_json::from_value(serde_json::to_value(config)?)?)
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
    use crate::config::{Backend, Memory, Theme};
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
    fn the_last_version_survives_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        assert_eq!(store.load_last_version().unwrap(), None);

        let version: Version = "0.2.1".parse().unwrap();
        store.save_last_version(&version).unwrap();
        drop(store);

        assert_eq!(
            Store::at(directory.path().to_owned())
                .unwrap()
                .load_last_version()
                .unwrap(),
            Some(version)
        );
    }

    #[test]
    fn state_from_the_first_release_is_adopted_once() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        // As 0.1.0 wrote it, every setting included.
        fs::write(
            directory.path().join(CONFIG_FILE),
            br#"{
  "mode": "fullscreen",
  "backend": "nvfbc",
  "codec": "av1",
  "buffer_seconds": 90,
  "frame_rate": 60,
  "bitrate_mbps": 50,
  "hotkey": "Alt+F10",
  "clip_directory": null,
  "theme": "classic",
  "audio": true,
  "cursor": false,
  "buffer_memory": "ram"
}"#,
        )
        .unwrap();
        let machine = Config {
            backend: Backend::Wgc,
            buffer_memory: Memory::Disk,
            ..Config::default()
        };

        assert!(store.adopt_first_release().unwrap());
        assert_eq!(
            store.load_last_version().unwrap(),
            Some(FIRST_RELEASE.parse().unwrap())
        );
        let config = store.load_config(&machine).unwrap();
        assert_eq!(config.backend, Backend::Wgc);
        assert_eq!(config.buffer_memory, Memory::Disk);
        assert_eq!(config.codec, crate::config::Codec::Av1);
        assert_eq!(config.buffer_seconds, 90);
        assert_eq!(config.hotkey.to_string(), "Alt+F10");

        assert!(!store.adopt_first_release().unwrap(), "only once");
    }

    #[test]
    fn a_first_release_choice_other_than_its_default_stays() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        fs::write(
            directory.path().join(CONFIG_FILE),
            br#"{"backend": "dxgi", "buffer_memory": "vram"}"#,
        )
        .unwrap();
        let machine = Config {
            backend: Backend::Wgc,
            buffer_memory: Memory::Disk,
            ..Config::default()
        };

        assert!(store.adopt_first_release().unwrap());
        let config = store.load_config(&machine).unwrap();
        assert_eq!(config.backend, Backend::Dxgi);
        assert_eq!(config.buffer_memory, Memory::Vram);
    }

    #[test]
    fn a_first_run_and_a_later_one_adopt_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        assert!(!store.adopt_first_release().unwrap());
        assert_eq!(store.load_last_version().unwrap(), None);

        store.save_totals(&Totals::default()).unwrap();
        store.save_last_version(&"0.2.0".parse().unwrap()).unwrap();
        assert!(!store.adopt_first_release().unwrap());
        assert_eq!(
            store.load_last_version().unwrap(),
            Some("0.2.0".parse().unwrap())
        );
    }

    #[test]
    fn the_last_run_reads_older_and_newer_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let file = directory.path().join(LAST_RUN_FILE);

        fs::write(&file, b"{}").unwrap();
        assert_eq!(store.load_last_version().unwrap(), None);
        fs::write(&file, br#"{"version":"0.2.0","from_the_future":1}"#).unwrap();
        assert_eq!(
            store.load_last_version().unwrap(),
            Some("0.2.0".parse().unwrap())
        );
        fs::write(&file, br#"{"version":"two"}"#).unwrap();
        assert_matches!(store.load_last_version(), Err(StorageError::Json(_)));
    }

    #[test]
    fn configuration_round_trips_and_lock_excludes_second_instance() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();

        assert_matches!(
            Store::at(directory.path().to_owned()),
            Err(StorageError::Locked)
        );
        assert_eq!(
            store.load_config(&Config::default()).unwrap(),
            Config::default()
        );

        let config = Config {
            buffer_seconds: 90,
            ..Config::default()
        };
        store.save_config(&Config::default(), &config).unwrap();

        drop(store);
        assert_eq!(
            Store::at(directory.path().to_owned())
                .unwrap()
                .load_config(&Config::default())
                .unwrap(),
            config
        );
    }

    #[test]
    fn settings_never_changed_follow_the_defaults_and_chosen_ones_stay() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let small = Config {
            buffer_memory: Memory::Disk,
            ..Config::default()
        };
        let large = Config {
            buffer_memory: Memory::Ram,
            backend: Backend::Wgc,
            ..Config::default()
        };

        let first = store.load_config(&small).unwrap();
        assert_eq!(first, small);
        let longer = Config {
            buffer_seconds: 120,
            ..first.clone()
        };
        store.save_config(&first, &longer).unwrap();
        store
            .save_config(
                &longer,
                &Config {
                    theme: Theme::Nova,
                    ..longer.clone()
                },
            )
            .unwrap();
        assert_eq!(
            store.load_config(&large).unwrap(),
            Config {
                buffer_seconds: 120,
                theme: Theme::Nova,
                ..large.clone()
            }
        );

        let vram = Config {
            buffer_memory: Memory::Vram,
            ..small.clone()
        };
        store.save_config(&small, &vram).unwrap();
        store
            .save_config(
                &vram,
                &Config {
                    buffer_memory: Memory::Disk,
                    ..vram.clone()
                },
            )
            .unwrap();
        let loaded = store.load_config(&large).unwrap();
        assert_eq!(
            loaded.buffer_memory,
            Memory::Disk,
            "chosen, then chosen back"
        );
        assert_eq!(loaded.backend, Backend::Wgc);
        assert_eq!(loaded.buffer_seconds, 120);
    }

    #[test]
    fn settings_a_newer_version_wrote_are_dropped_on_save() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        fs::write(
            directory.path().join(CONFIG_FILE),
            br#"{"frame_rate":30,"from_the_future":1}"#,
        )
        .unwrap();
        let config = store.load_config(&Config::default()).unwrap();
        assert_eq!(config.frame_rate, 30);

        store
            .save_config(
                &config,
                &Config {
                    audio: false,
                    ..config.clone()
                },
            )
            .unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(directory.path().join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(saved, serde_json::json!({"frame_rate": 30, "audio": false}));
    }

    #[test]
    fn corrupt_state_is_not_silently_reset() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        fs::write(directory.path().join(CONFIG_FILE), b"broken").unwrap();
        assert_matches!(
            store.load_config(&Config::default()),
            Err(StorageError::Json(_))
        );
        fs::write(directory.path().join(CONFIG_FILE), br#"{"frame_rate":999}"#).unwrap();
        assert_matches!(
            store.load_config(&Config::default()),
            Err(StorageError::Json(_))
        );
    }

    #[test]
    fn totals_read_older_and_newer_files() {
        let parse = |json: &str| serde_json::from_str::<Totals>(json).unwrap();
        let totals = Totals {
            buffered_bytes: 5,
            saved_bytes: 7,
        };
        assert_eq!(parse(r#"{"buffered_bytes":5,"written_bytes":7}"#), totals);
        assert_eq!(
            parse(r#"{"buffered_bytes":5,"saved_bytes":7,"from_the_future":1}"#),
            totals
        );
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
