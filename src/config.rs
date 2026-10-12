//! The user's settings: what to record, how it is captured, codec, buffer length, frame rate,
//! bitrate, whether to record sound, the hotkey that saves a clip and the clip folder. They are
//! stored as JSON and checked before use, since the file can be edited by hand: the buffer
//! covers 5 to 600 seconds, capture runs at 24 to 240 fps and the bitrate is 5 to 150 Mbps.
//! Clips go to `%USERPROFILE%\Videos\hindsight` unless another folder is chosen.

use crate::sys::hotkey::{Hotkey, ParseError};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};
use thiserror::Error;

pub(crate) const MIN_BUFFER_SECONDS: u32 = 5;
pub(crate) const MAX_BUFFER_SECONDS: u32 = 600;
pub(crate) const MIN_FRAME_RATE: u32 = 24;
pub(crate) const MAX_FRAME_RATE: u32 = 240;
pub(crate) const MIN_BITRATE_MBPS: u32 = 5;
pub(crate) const MAX_BITRATE_MBPS: u32 = 150;
const BITS_PER_MEGABIT: u32 = 1_000_000;
const BITS_PER_BYTE: u64 = 8;
const CLIP_DIRECTORY_ENV: &str = "USERPROFILE";
const VIDEOS_DIRECTORY: &str = "Videos";
const CLIP_DIRECTORY: &str = "hindsight";

/// What is recorded: any app that fills its monitor, while one is in front, or the primary
/// monitor the whole time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    #[default]
    Fullscreen,
    Desktop,
}

impl Mode {
    pub(crate) const ALL: [Self; 2] = [Self::Fullscreen, Self::Desktop];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Fullscreen => "Full-screen apps",
            Self::Desktop => "Whole desktop, always",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// How frames are captured. Windows Graphics Capture can record one app's window; DXGI Desktop
/// Duplication and NVIDIA's NvFBC see whole monitors only, so a full-screen app is recorded
/// through its monitor. NvFBC is for licensed vendors: it needs the key NVIDIA issued them. It
/// comes in two forms: grabbing into Direct3D 9 surfaces, or into CUDA memory that the encoder
/// reads in place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    #[default]
    Wgc,
    Dxgi,
    Nvfbc,
    #[serde(rename = "nvfbc_cuda")]
    NvfbcCuda,
}

impl Backend {
    pub(crate) const ALL: [Self; 4] = [Self::Wgc, Self::Dxgi, Self::Nvfbc, Self::NvfbcCuda];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Wgc => "Windows Graphics Capture",
            Self::Dxgi => "DXGI Desktop Duplication",
            Self::Nvfbc => "NVIDIA NvFBC",
            Self::NvfbcCuda => "NVIDIA NvFBC to CUDA",
        }
    }

    /// Whether the backend captures with NvFBC, and so needs the key NVIDIA issued.
    pub(crate) const fn uses_nvfbc(self) -> bool {
        matches!(self, Self::Nvfbc | Self::NvfbcCuda)
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The video codecs NVENC offers, as Instant Replay does. AV1 needs an RTX 40 series GPU or
/// newer.
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

impl fmt::Display for Codec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Where the replay buffer is kept: in system memory, or in the GPU's own, which leaves the
/// system's free for the game and costs the GPU's instead. A save reads a buffer in GPU memory
/// back through system memory, so for a moment it takes as much of both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Memory {
    #[default]
    Ram,
    Vram,
}

impl Memory {
    pub(crate) const ALL: [Self; 2] = [Self::Ram, Self::Vram];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ram => "System memory (RAM)",
            Self::Vram => "GPU memory (VRAM)",
        }
    }

    /// The short name, for log lines and estimates.
    pub(crate) const fn short_name(self) -> &'static str {
        match self {
            Self::Ram => "RAM",
            Self::Vram => "VRAM",
        }
    }
}

impl fmt::Display for Memory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// How the window looks. Classic is the plain Windows dialog; the others dress it in a
/// character's colours and artwork.
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

impl fmt::Display for Theme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Every setting, as stored; a field the file leaves out takes its default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) mode: Mode,
    pub(crate) backend: Backend,
    pub(crate) codec: Codec,
    /// Seconds of gameplay a saved clip covers, at least.
    pub(crate) buffer_seconds: u32,
    pub(crate) frame_rate: u32,
    pub(crate) bitrate_mbps: u32,
    /// The chord that saves a clip, such as `Alt+F10`.
    pub(crate) hotkey: String,
    /// Where clips are saved; empty for the default folder.
    pub(crate) clip_directory: String,
    /// The window's look; applied as soon as it is chosen, without restarting recording.
    pub(crate) theme: Theme,
    /// Whether clips carry the system's sound, as the default output device plays it.
    pub(crate) audio: bool,
    /// Whether the mouse cursor is drawn into the picture, where the backend can (NvFBC and
    /// Windows Graphics Capture; Desktop Duplication never draws it).
    pub(crate) cursor: bool,
    pub(crate) buffer_memory: Memory,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Fullscreen,
            backend: Backend::Wgc,
            codec: Codec::H264,
            buffer_seconds: 30,
            frame_rate: 60,
            bitrate_mbps: 50,
            hotkey: "Alt+F10".to_owned(),
            clip_directory: String::new(),
            theme: Theme::Classic,
            audio: true,
            cursor: false,
            buffer_memory: Memory::Ram,
        }
    }
}

impl Config {
    /// Checks the settings a hand-edited file could get wrong, reporting the first bad one.
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
        self.parsed_hotkey()?;
        if !self.clip_directory.is_empty() && !is_absolute_directory(&self.clip_directory) {
            return Err(ConfigError::ClipDirectory);
        }
        Ok(())
    }

    pub(crate) fn parsed_hotkey(&self) -> Result<Hotkey, ConfigError> {
        Ok(self.hotkey.parse()?)
    }

    /// The bitrate in bits per second. Saturates rather than overflows, since the Settings tab
    /// estimates the buffer from whatever is typed before the value is checked.
    pub(crate) const fn bitrate(&self) -> u32 {
        self.bitrate_mbps.saturating_mul(BITS_PER_MEGABIT)
    }

    /// Frames the buffer keeps, at least: the clip length at the capture rate.
    pub(crate) const fn buffer_frames(&self) -> u64 {
        self.buffer_seconds as u64 * self.frame_rate as u64
    }

    /// Where clips are saved: the chosen folder, or the default one.
    pub(crate) fn clip_directory(&self) -> PathBuf {
        if self.clip_directory.is_empty() {
            default_clip_directory()
        } else {
            PathBuf::from(&self.clip_directory)
        }
    }

    /// Encoded bytes the buffer holds at the configured bitrate, for the Settings estimate.
    pub(crate) const fn estimated_buffer_bytes(&self) -> u64 {
        self.buffer_seconds as u64 * self.bitrate() as u64 / BITS_PER_BYTE
    }
}

/// Why a setting was rejected.
#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error(
        "buffer length must be between {} and {} seconds",
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
    #[error(transparent)]
    Hotkey(#[from] ParseError),
    #[error("the clip folder must be an absolute path such as D:\\Clips")]
    ClipDirectory,
}

/// `%USERPROFILE%\Videos\hindsight`, or the temporary directory when there is no profile.
pub(crate) fn default_clip_directory() -> PathBuf {
    std::env::var_os(CLIP_DIRECTORY_ENV)
        .map_or_else(std::env::temp_dir, |profile| {
            PathBuf::from(profile).join(VIDEOS_DIRECTORY)
        })
        .join(CLIP_DIRECTORY)
}

/// A drive-rooted Windows path (`C:\Clips`) or a UNC share (`\\server\share\Clips`).
fn is_absolute_directory(value: &str) -> bool {
    let mut chars = value.chars();
    let on_drive = matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(letter), Some(':'), Some('\\')) if letter.is_ascii_alphabetic()
    );
    let on_share = value.starts_with(r"\\") && value.len() > 2;

    (on_drive || on_share) && !value.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn defaults_fill_missing_settings_without_accepting_unknown_ones() {
        let parse = |json: &str| serde_json::from_str::<Config>(json);

        let config = parse(r#"{"buffer_seconds":120,"codec":"av1"}"#).unwrap();
        assert_eq!(config.mode, Mode::Fullscreen);
        assert_eq!(config.backend, Backend::Wgc);
        assert_eq!(config.codec, Codec::Av1);
        assert_eq!(config.buffer_seconds, 120);
        assert_eq!(config.frame_rate, 60);
        assert_eq!(config.hotkey, "Alt+F10");
        assert_eq!(config.theme, Theme::Classic);
        // Settings saved before sound was recorded turn it on.
        assert!(config.audio);
        assert_eq!(parse("{}").unwrap(), Config::default());
        assert!(!parse(r#"{"audio":false}"#).unwrap().audio);
        // The cursor stays out of the picture unless asked for.
        assert!(!config.cursor);
        assert!(parse(r#"{"cursor":true}"#).unwrap().cursor);
        // The buffer stays in system memory unless asked to go to the GPU's.
        assert_eq!(config.buffer_memory, Memory::Ram);
        assert_eq!(
            parse(r#"{"buffer_memory":"vram"}"#).unwrap().buffer_memory,
            Memory::Vram
        );
        assert!(parse(r#"{"buffer_memory":"gpu"}"#).is_err());

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
        assert!(parse(r#"{"buffer_secs":120}"#).is_err());
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
            assert_eq!(
                backend.uses_nvfbc(),
                backend.name().contains("NvFBC"),
                "{backend:?}"
            );
        }
        assert!(
            serde_json::to_string(&Backend::NvfbcCuda)
                .unwrap()
                .contains("nvfbc_cuda")
        );
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

        config.hotkey = "F".to_owned();
        assert_matches!(config.validate(), Err(ConfigError::Hotkey(_)));
        config.hotkey = "Ctrl+Shift+S".to_owned();

        config.clip_directory = r"Clips\here".to_owned();
        assert_matches!(config.validate(), Err(ConfigError::ClipDirectory));
        config.clip_directory = r"D:\Clips".to_owned();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn clip_folders_are_drive_rooted_or_shares() {
        for path in [
            r"C:\Clips",
            r"d:\Videos\Replays",
            r"\\nas\clips",
            r"C:\影像",
        ] {
            assert!(is_absolute_directory(path), "{path}");
        }
        for path in ["", "C:", r"C:Clips", "C:/Clips", r"\Clips", r"\\", "Clips"] {
            assert!(!is_absolute_directory(path), "{path}");
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
                clip_directory: r"D:\Clips".to_owned(),
                ..Config::default()
            }
            .clip_directory(),
            PathBuf::from(r"D:\Clips")
        );
    }

    #[test]
    fn an_absurd_bitrate_typed_into_the_estimate_does_not_overflow() {
        // The Settings tab estimates from the typed value before it is validated.
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
