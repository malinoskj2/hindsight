//! System sound for the clips: what the default output device plays, captured through WASAPI
//! loopback on a thread of its own and handed to the session's buffer as 16-bit PCM numbered on
//! the clock the video frames use (the replay module's `audio` describes that clock), then
//! encoded to AAC-LC at 192 kb/s when a clip is saved, for the clip's span only.
//!
//! The sound never holds up the video. It runs on its own thread, which takes the buffer's lock
//! only to append what it read; a device that goes away or stops being the default is reopened
//! by that thread alone, with silence filling the time in between; and a session whose sound
//! cannot be captured at all records its video without it, as does a save whose sound cannot
//! be encoded.
//!
//! The sample conversion and the timeline are portable and tested everywhere; the capture, the
//! encoder and the thread that drives them are Windows-only.

#[cfg(windows)]
#[expect(unsafe_code)]
mod aac;
#[cfg(windows)]
#[expect(unsafe_code)]
mod loopback;
mod pcm;
#[cfg(windows)]
mod recorder;
mod timeline;

#[cfg(windows)]
pub(crate) use recorder::{Capture, start};

use crate::replay::{AudioClip, PcmClip};
use thiserror::Error;

/// The sample rate the sound is captured and encoded at, which Windows converts other rates to.
const SAMPLE_RATE: u32 = 48_000;
/// Stereo: Windows mixes more channels down, and mono is played on both sides.
const CHANNELS: u16 = 2;

/// Why a clip's sound could not be encoded.
#[derive(Debug, Error)]
pub(crate) enum EncodeError {
    /// COM, Media Foundation or its AAC encoder could not be started, or the encoder failed.
    #[cfg(windows)]
    #[error("the AAC encoder failed ({0})")]
    Windows(#[from] windows::core::Error),
    /// The encoder finished fewer frames than the clip takes.
    #[cfg(windows)]
    #[error("the AAC encoder made {made} of the clip's {needed} frames")]
    Short { made: usize, needed: usize },
    /// There is no AAC encoder to use elsewhere.
    #[cfg(not(windows))]
    #[error("sound is only encoded on Windows")]
    Unsupported,
}

/// Encodes a saved clip's sound to AAC on the calling thread.
#[cfg(windows)]
pub(crate) fn encode(clip: &PcmClip) -> Result<AudioClip, EncodeError> {
    aac::encode_clip(clip)
}

/// Elsewhere there is no encoder; nothing records sound to encode either.
#[cfg(not(windows))]
pub(crate) fn encode(_clip: &PcmClip) -> Result<AudioClip, EncodeError> {
    Err(EncodeError::Unsupported)
}

/// Elsewhere there is no sound to capture.
#[cfg(not(windows))]
pub(crate) struct Capture;

/// Elsewhere there is no sound to capture, so sessions record video only.
#[cfg(not(windows))]
pub(crate) fn start(_live: &std::sync::Arc<crate::replay::Live>) -> Option<Capture> {
    None
}
