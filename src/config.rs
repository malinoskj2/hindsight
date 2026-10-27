//! The user's settings, stored as JSON and validated before use.

use crate::{log, sys::hotkey::Hotkey};
use serde::{Deserialize, Deserializer, Serialize, de::IgnoredAny};
use std::{collections::BTreeMap, fmt, path::PathBuf};
use thiserror::Error;

pub(crate) const MIN_BUFFER_SECONDS: u32 = 5;
pub(crate) const MAX_BUFFER_SECONDS: u32 = 600;
pub(crate) const MIN_FRAME_RATE: u32 = 24;
pub(crate) const MAX_FRAME_RATE: u32 = 240;
pub(crate) const MIN_BITRATE_MBPS: u32 = 5;
pub(crate) const MAX_BITRATE_MBPS: u32 = 150;
const BITS_PER_MEGABIT: u32 = 1_000_000;
const BITS_PER_BYTE: u64 = 8;
const CLIP_DIRECTORY: &str = "hindsight";
/// Installed memory from which the buffer defaults to RAM. Games of this generation want 8 to
/// 16 GB of their own.
const RAM_BUFFER_MEMORY: u64 = 32 << 30;
/// Video memory from which the buffer defaults to VRAM. A 12 GB card reports a little under
/// 12 GiB; on a card with 8 GB, a buffer pushes the game's textures out.
const VRAM_BUFFER_MEMORY: u64 = 11 << 30;

macro_rules! display_by_name {
    ($($choice:ty),*) => {$(
        impl fmt::Display for $choice {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.name())
            }
        }
    )*};
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    #[default]
    Fullscreen,
    Desktop,
}

impl Mode {
    #[cfg(windows)]
    pub(crate) const ALL: [Self; 2] = [Self::Fullscreen, Self::Desktop];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Fullscreen => "Full-screen apps",
            Self::Desktop => "Whole desktop, always",
        }
    }
}

/// The capture backend. NvFBC needs NVIDIA's vendor key. `NvfbcCuda` captures into CUDA memory
/// that the encoder reads in place, and is offered only when the experiment switch is set, since
/// it costs twice NvFBC's CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    Wgc,
    Dxgi,
    #[default]
    Nvfbc,
    #[serde(rename = "nvfbc_cuda")]
    NvfbcCuda,
}

impl Backend {
    // Ordered from the cheapest for the game.
    pub(crate) const ALL: [Self; 4] = [Self::Nvfbc, Self::NvfbcCuda, Self::Wgc, Self::Dxgi];
    /// The environment variable that, when set to anything, offers the CUDA backend in Settings.
    #[cfg(windows)]
    pub(crate) const EXPERIMENT_SWITCH: &str = "HINDSIGHT_CUDA_BACKEND";

    /// Returns the backends Settings lists: every one except the CUDA experiment, unless the
    /// switch is set or the config already chose it.
    #[cfg(windows)]
    pub(crate) fn offered(chosen: Self) -> Vec<Self> {
        let experiments = std::env::var_os(Self::EXPERIMENT_SWITCH).is_some();
        Self::offered_with(chosen, experiments)
    }

    fn offered_with(chosen: Self, experiments: bool) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|backend| {
                *backend != Self::NvfbcCuda || experiments || chosen == Self::NvfbcCuda
            })
            .collect()
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Wgc => "Windows Graphics Capture",
            Self::Dxgi => "DXGI Desktop Duplication",
            Self::Nvfbc => "NVIDIA NvFBC",
            Self::NvfbcCuda => "NVIDIA NvFBC to CUDA",
        }
    }

    /// Whether a rotated monitor is captured upright. NvFBC and Desktop Duplication deliver the
    /// panel's own orientation, so they refuse a rotated monitor.
    pub(crate) fn captures_rotated(self) -> bool {
        self == Self::Wgc
    }
}

/// AV1 needs an RTX 40 series GPU or newer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Codec {
    #[default]
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub(crate) const ALL: [Self; 3] = [Self::H264, Self::Hevc, Self::Av1];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Memory {
    #[default]
    Ram,
    Vram,
    Disk,
}

impl Memory {
    pub(crate) const ALL: [Self; 3] = [Self::Ram, Self::Vram, Self::Disk];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ram => "System memory (RAM)",
            Self::Vram => "GPU memory (VRAM)",
            Self::Disk => "Disk (SSD)",
        }
    }

    pub(crate) const fn place(self) -> &'static str {
        match self {
            Self::Ram => "in RAM",
            Self::Vram => "in VRAM",
            Self::Disk => "on disk",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    #[default]
    Classic,
    Ember,
    Selene,
    Nova,
}

impl Theme {
    #[cfg(windows)]
    pub(crate) const ALL: [Self; 4] = [Self::Classic, Self::Ember, Self::Selene, Self::Nova];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Classic => "Classic Windows",
            Self::Ember => "Ember, dragon maid",
            Self::Selene => "Selene, moon-moth archivist",
            Self::Nova => "Nova, arcade pilot",
        }
    }
}

display_by_name!(Mode, Backend, Codec, Memory, Theme);

/// The machine, as far as the defaults depend on it. Each part is `None` when it could not be
/// read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Machine {
    /// Installed system memory, in bytes.
    pub(crate) memory: Option<u64>,
    /// The NVIDIA GPU's dedicated memory, in bytes.
    pub(crate) video_memory: Option<u64>,
    pub(crate) nvidia_drives_display: Option<bool>,
}

impl Machine {
    /// NvFBC and Desktop Duplication see only displays attached to the NVIDIA GPU, which a laptop
    /// whose display hangs off the integrated GPU has none of.
    fn backend(self) -> Backend {
        if self.nvidia_drives_display == Some(false) {
            Backend::Wgc
        } else {
            Backend::Nvfbc
        }
    }

    fn buffer_memory(self) -> Memory {
        if self.memory.is_some_and(|bytes| bytes >= RAM_BUFFER_MEMORY) {
            Memory::Ram
        } else if self
            .video_memory
            .is_some_and(|bytes| bytes >= VRAM_BUFFER_MEMORY)
        {
            Memory::Vram
        } else {
            Memory::Disk
        }
    }
}

/// The validated settings. A `Config` read from disk is built only from settings that passed
/// [`Config::validate`], and the Settings tab validates what it builds from the form the same
/// way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConfig")]
pub(crate) struct Config {
    pub(crate) mode: Mode,
    pub(crate) backend: Backend,
    pub(crate) codec: Codec,
    pub(crate) buffer_seconds: u32,
    pub(crate) frame_rate: u32,
    pub(crate) bitrate_mbps: u32,
    pub(crate) hotkey: Hotkey,
    /// `None` for the default folder.
    pub(crate) clip_directory: Option<PathBuf>,
    pub(crate) theme: Theme,
    /// Whether the system's sound is recorded.
    pub(crate) audio: bool,
    /// Whether the default microphone is recorded, mixed into the clip's sound.
    pub(crate) microphone: bool,
    /// Whether the cursor is drawn, where the backend can draw it. Desktop Duplication never
    /// draws it.
    pub(crate) cursor: bool,
    pub(crate) buffer_memory: Memory,
}

/// The settings as stored, before validation. Missing settings take their defaults.
#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    mode: Mode,
    backend: Backend,
    codec: Codec,
    buffer_seconds: u32,
    frame_rate: u32,
    bitrate_mbps: u32,
    hotkey: Hotkey,
    /// Older files wrote the default folder as an empty string.
    #[serde(deserialize_with = "folder_or_none")]
    clip_directory: Option<PathBuf>,
    theme: Theme,
    audio: bool,
    microphone: bool,
    cursor: bool,
    buffer_memory: Memory,
    /// Settings written by a newer version, collected here so a downgrade still starts.
    #[serde(flatten)]
    #[expect(
        clippy::zero_sized_map_values,
        reason = "flatten collects into a map; only the keys are kept"
    )]
    unknown: BTreeMap<String, IgnoredAny>,
}

impl Default for RawConfig {
    fn default() -> Self {
        let Config {
            mode,
            backend,
            codec,
            buffer_seconds,
            frame_rate,
            bitrate_mbps,
            hotkey,
            clip_directory,
            theme,
            audio,
            microphone,
            cursor,
            buffer_memory,
        } = Config::default();
        Self {
            mode,
            backend,
            codec,
            buffer_seconds,
            frame_rate,
            bitrate_mbps,
            hotkey,
            clip_directory,
            theme,
            audio,
            microphone,
            cursor,
            buffer_memory,
            unknown: BTreeMap::new(),
        }
    }
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigError;

    fn try_from(raw: RawConfig) -> Result<Self, ConfigError> {
        let RawConfig {
            mode,
            backend,
            codec,
            buffer_seconds,
            frame_rate,
            bitrate_mbps,
            hotkey,
            clip_directory,
            theme,
            audio,
            microphone,
            cursor,
            buffer_memory,
            unknown,
        } = raw;
        if !unknown.is_empty() {
            let keys: Vec<&str> = unknown.keys().map(String::as_str).collect();
            log::warning(format!(
                "Settings · unknown keys ignored · {}",
                keys.join(", ")
            ));
        }
        let config = Self {
            mode,
            backend,
            codec,
            buffer_seconds,
            frame_rate,
            bitrate_mbps,
            hotkey,
            clip_directory,
            theme,
            audio,
            microphone,
            cursor,
            buffer_memory,
        };
        config.validate()?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Fullscreen,
            backend: Backend::Nvfbc,
            codec: Codec::H264,
            buffer_seconds: 30,
            frame_rate: 60,
            bitrate_mbps: 50,
            hotkey: Hotkey::DEFAULT,
            clip_directory: None,
            theme: Theme::Classic,
            audio: true,
            microphone: false,
            cursor: false,
            buffer_memory: Memory::Ram,
        }
    }
}

impl Config {
    /// Returns the defaults for the settings that depend on the machine.
    pub(crate) fn for_machine(machine: Machine) -> Self {
        Self {
            backend: machine.backend(),
            buffer_memory: machine.buffer_memory(),
            ..Self::default()
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if !(MIN_BUFFER_SECONDS..=MAX_BUFFER_SECONDS).contains(&self.buffer_seconds) {
            return Err(ConfigError::Buffer);
        }
        if !(MIN_FRAME_RATE..=MAX_FRAME_RATE).contains(&self.frame_rate) {
            return Err(ConfigError::FrameRate);
        }
        if !(MIN_BITRATE_MBPS..=MAX_BITRATE_MBPS).contains(&self.bitrate_mbps) {
            return Err(ConfigError::Bitrate);
        }
        if let Some(folder) = &self.clip_directory
            && !folder.is_absolute()
        {
            return Err(ConfigError::ClipDirectory);
        }
        Ok(())
    }

    /// Saturates on overflow, since the Settings tab estimates from unvalidated input.
    pub(crate) const fn bitrate(&self) -> u32 {
        self.bitrate_mbps.saturating_mul(BITS_PER_MEGABIT)
    }

    pub(crate) const fn buffer_frames(&self) -> u64 {
        self.buffer_seconds as u64 * self.frame_rate as u64
    }

    pub(crate) fn clip_directory(&self) -> PathBuf {
        self.clip_directory
            .as_ref()
            .map_or_else(default_clip_directory, PathBuf::clone)
    }

    pub(crate) const fn estimated_buffer_bytes(&self) -> u64 {
        self.buffer_seconds as u64 * self.bitrate() as u64 / BITS_PER_BYTE
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error(
        "replay length must be between {} and {} seconds",
        MIN_BUFFER_SECONDS,
        MAX_BUFFER_SECONDS
    )]
    Buffer,
    #[error(
        "frame rate must be between {} and {} fps",
        MIN_FRAME_RATE,
        MAX_FRAME_RATE
    )]
    FrameRate,
    #[error(
        "bitrate must be between {} and {} Mbps",
        MIN_BITRATE_MBPS,
        MAX_BITRATE_MBPS
    )]
    Bitrate,
    #[error("the clip folder must be an absolute path such as D:\\Clips")]
    ClipDirectory,
}

fn folder_or_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<PathBuf>, D::Error> {
    Ok(Option::<PathBuf>::deserialize(deserializer)?
        .filter(|folder| !folder.as_os_str().is_empty()))
}

/// Returns a folder in the user's Videos library, or in the temporary directory when there is
/// no library.
pub(crate) fn default_clip_directory() -> PathBuf {
    videos_directory()
        .unwrap_or_else(std::env::temp_dir)
        .join(CLIP_DIRECTORY)
}

/// Returns the Videos library as the shell reports it, which follows the library when it is
/// moved or redirected.
#[cfg(windows)]
fn videos_directory() -> Option<PathBuf> {
    crate::sys::com::known_folder(&winsafe::co::KNOWNFOLDERID::Videos)
}

#[cfg(not(windows))]
fn videos_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|profile| PathBuf::from(profile).join("Videos"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn defaults_fill_missing_settings_and_unknown_ones_are_ignored() {
        let parse = |json: &str| serde_json::from_str::<Config>(json);

        let config = parse(r#"{"buffer_seconds":120,"codec":"av1"}"#).unwrap();
        assert_eq!(config.mode, Mode::Fullscreen);
        assert_eq!(config.backend, Backend::Nvfbc);
        assert_eq!(config.codec, Codec::Av1);
        assert_eq!(config.buffer_seconds, 120);
        assert_eq!(config.frame_rate, 60);
        assert_eq!(config.hotkey.to_string(), "Alt+F10");
        assert_eq!(config.theme, Theme::Classic);
        assert!(config.audio);
        assert_eq!(parse("{}").unwrap(), Config::default());
        assert!(!parse(r#"{"audio":false}"#).unwrap().audio);
        assert!(!config.microphone);
        assert!(parse(r#"{"microphone":true}"#).unwrap().microphone);
        assert!(!config.cursor);
        assert!(parse(r#"{"cursor":true}"#).unwrap().cursor);
        assert_eq!(config.buffer_memory, Memory::Ram);
        assert_eq!(
            parse(r#"{"buffer_memory":"vram"}"#).unwrap().buffer_memory,
            Memory::Vram
        );
        assert_eq!(
            parse(r#"{"buffer_memory":"disk"}"#).unwrap().buffer_memory,
            Memory::Disk
        );
        assert!(parse(r#"{"buffer_memory":"gpu"}"#).is_err());
        assert!(parse(r#"{"buffer_memory":"ssd"}"#).is_err());

        assert_eq!(parse(r#"{"mode":"desktop"}"#).unwrap().mode, Mode::Desktop);
        assert_eq!(
            parse(r#"{"backend":"dxgi"}"#).unwrap().backend,
            Backend::Dxgi
        );
        assert_eq!(
            parse(r#"{"backend":"nvfbc"}"#).unwrap().backend,
            Backend::Nvfbc
        );
        assert_eq!(
            parse(r#"{"backend":"nvfbc_cuda"}"#).unwrap().backend,
            Backend::NvfbcCuda
        );
        assert!(parse(r#"{"backend":"nvfbccuda"}"#).is_err());
        assert_eq!(parse(r#"{"theme":"selene"}"#).unwrap().theme, Theme::Selene);

        assert!(parse(r#"{"backend":"gdi"}"#).is_err());
        assert!(parse(r#"{"codec":"vp9"}"#).is_err());
        assert!(parse(r#"{"theme":"neon"}"#).is_err());
        assert_eq!(
            parse(r#"{"buffer_secs":120,"frame_rate":30}"#).unwrap(),
            Config {
                frame_rate: 30,
                ..Config::default()
            }
        );
    }

    #[test]
    fn the_hotkey_and_the_clip_folder_are_read_as_text() {
        let parse = |json: &str| serde_json::from_str::<Config>(json);

        let config = parse(r#"{"hotkey":"ctrl+shift+s"}"#).unwrap();
        assert_eq!(config.hotkey.to_string(), "Ctrl+Shift+S");
        assert!(parse(r#"{"hotkey":"F"}"#).is_err());
        assert!(parse(r#"{"hotkey":""}"#).is_err());
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""hotkey":"Ctrl+Shift+S""#), "{json}");
        assert_eq!(parse(&json).unwrap(), config);

        assert_eq!(
            parse(r#"{"clip_directory":""}"#).unwrap().clip_directory,
            None
        );
        assert_eq!(
            parse(r#"{"clip_directory":null}"#).unwrap().clip_directory,
            None
        );
        let folder = absolute_folder();
        let config = parse(&format!(r#"{{"clip_directory":{}}}"#, json_string(&folder))).unwrap();
        assert_eq!(config.clip_directory, Some(folder));
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(parse(&json).unwrap(), config);
    }

    #[test]
    fn settings_out_of_range_are_refused_when_read() {
        let parse = |json: &str| serde_json::from_str::<Config>(json);

        for json in [
            r#"{"buffer_seconds":601}"#,
            r#"{"frame_rate":23}"#,
            r#"{"bitrate_mbps":0}"#,
            r#"{"clip_directory":"Clips"}"#,
        ] {
            assert!(parse(json).is_err(), "{json}");
        }
        let error = parse(r#"{"frame_rate":241}"#).unwrap_err().to_string();
        assert!(
            error.starts_with(&ConfigError::FrameRate.to_string()),
            "{error}"
        );
    }

    #[test]
    fn the_buffer_defaults_to_ram_then_vram_then_disk_as_the_machine_has_room() {
        const GIB: u64 = 1 << 30;
        let buffer = |memory, video_memory| {
            Config::for_machine(Machine {
                memory,
                video_memory,
                nvidia_drives_display: Some(true),
            })
            .buffer_memory
        };

        assert_eq!(buffer(Some(32 * GIB), Some(8 * GIB)), Memory::Ram);
        assert_eq!(buffer(Some(64 * GIB), None), Memory::Ram);
        assert_eq!(
            buffer(Some(16 * GIB), Some(12 * GIB - (200 << 20))),
            Memory::Vram
        );
        assert_eq!(buffer(None, Some(24 * GIB)), Memory::Vram);
        assert_eq!(buffer(Some(16 * GIB), Some(8 * GIB)), Memory::Disk);
        assert_eq!(buffer(Some(32 * GIB - 1), Some(10 * GIB)), Memory::Disk);
        assert_eq!(buffer(None, None), Memory::Disk);
    }

    #[test]
    fn the_backend_defaults_to_nvfbc_unless_no_display_hangs_off_the_nvidia_gpu() {
        let backend = |nvidia_drives_display| {
            Config::for_machine(Machine {
                nvidia_drives_display,
                ..Machine::default()
            })
            .backend
        };

        assert_eq!(backend(Some(true)), Backend::Nvfbc);
        assert_eq!(backend(None), Backend::Nvfbc);
        assert_eq!(backend(Some(false)), Backend::Wgc);
    }

    #[test]
    fn the_cuda_backend_is_offered_only_by_switch_or_when_already_chosen() {
        let without = [Backend::Nvfbc, Backend::Wgc, Backend::Dxgi];
        assert_eq!(Backend::offered_with(Backend::Nvfbc, false), without);
        assert_eq!(Backend::offered_with(Backend::Nvfbc, true), Backend::ALL);
        assert_eq!(
            Backend::offered_with(Backend::NvfbcCuda, false),
            Backend::ALL
        );
    }

    #[test]
    fn backends_round_trip_and_the_nvfbc_ones_need_the_key() {
        for backend in Backend::ALL {
            let config = Config {
                backend,
                ..Config::default()
            };
            let json = serde_json::to_string(&config).unwrap();
            assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
        }
        assert!(
            serde_json::to_string(&Backend::NvfbcCuda)
                .unwrap()
                .contains("nvfbc_cuda")
        );
    }

    #[test]
    fn buffer_places_round_trip_under_their_own_names() {
        for (memory, name) in Memory::ALL.into_iter().zip(["ram", "vram", "disk"]) {
            assert_eq!(
                serde_json::to_string(&memory).unwrap(),
                format!("\"{name}\"")
            );
            let config = Config {
                buffer_memory: memory,
                ..Config::default()
            };
            let json = serde_json::to_string(&config).unwrap();
            assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
        }
    }

    #[test]
    fn rejects_out_of_range_settings() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());

        config.buffer_seconds = MAX_BUFFER_SECONDS + 1;
        assert_matches!(config.validate(), Err(ConfigError::Buffer));
        config.buffer_seconds = 30;

        config.frame_rate = MIN_FRAME_RATE - 1;
        assert_matches!(config.validate(), Err(ConfigError::FrameRate));
        config.frame_rate = 60;

        config.bitrate_mbps = 0;
        assert_matches!(config.validate(), Err(ConfigError::Bitrate));
        config.bitrate_mbps = 50;

        config.clip_directory = Some(PathBuf::from(r"Clips\here"));
        assert_matches!(config.validate(), Err(ConfigError::ClipDirectory));
        config.clip_directory = Some(absolute_folder());
        assert!(config.validate().is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn clip_folders_are_drive_rooted_or_shares() {
        let valid = |path: &str| {
            Config {
                clip_directory: Some(PathBuf::from(path)),
                ..Config::default()
            }
            .validate()
            .is_ok()
        };
        for path in [
            r"C:\Clips",
            r"d:\Videos\Replays",
            r"\\nas\clips",
            r"C:\影像",
            "C:/Clips",
        ] {
            assert!(valid(path), "{path}");
        }
        for path in ["C:", r"C:Clips", r"\Clips", "Clips"] {
            assert!(!valid(path), "{path}");
        }
    }

    #[test]
    fn derived_values_follow_the_settings() {
        let config = Config::default();

        assert_eq!(config.bitrate(), 50_000_000);
        assert_eq!(config.buffer_frames(), 1800);
        assert_eq!(config.estimated_buffer_bytes(), 187_500_000);
        assert!(config.clip_directory().ends_with(CLIP_DIRECTORY));
        assert_eq!(
            Config {
                clip_directory: Some(absolute_folder()),
                ..Config::default()
            }
            .clip_directory(),
            absolute_folder()
        );
    }

    /// An absolute folder on the platform the tests run on.
    fn absolute_folder() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"D:\Clips" } else { "/clips" })
    }

    fn json_string(path: &std::path::Path) -> String {
        serde_json::to_string(path).unwrap()
    }

    #[test]
    fn an_absurd_bitrate_typed_into_the_estimate_does_not_overflow() {
        let config = Config {
            bitrate_mbps: u32::MAX,
            buffer_seconds: MAX_BUFFER_SECONDS,
            ..Config::default()
        };
        assert_eq!(config.bitrate(), u32::MAX);
        assert_eq!(
            config.estimated_buffer_bytes(),
            u64::from(MAX_BUFFER_SECONDS) * u64::from(u32::MAX) / BITS_PER_BYTE
        );
    }
}
