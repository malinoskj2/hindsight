//! System sound: WASAPI loopback capture on its own thread, stamped on the video clock and
//! encoded to AAC-LC when a clip is saved.

#[cfg(windows)]
#[expect(unsafe_code)]
mod aac;
#[cfg(windows)]
#[expect(unsafe_code)]
mod loopback;
mod pcm;
#[cfg(windows)]
mod recorder;
#[cfg(test)]
pub(crate) mod testing;
mod timeline;

#[cfg(windows)]
pub(crate) use recorder::{Capture, start};

use crate::replay::{AudioClip, PcmClip};
use thiserror::Error;

/// Windows resamples other rates to this.
#[cfg_attr(all(not(windows), test), expect(dead_code))]
const SAMPLE_RATE: u32 = 48_000;
#[cfg_attr(all(not(windows), test), expect(dead_code))]
const CHANNELS: u16 = 2;

#[derive(Debug, Error)]
pub(crate) enum EncodeError {
    #[cfg(windows)]
    #[error("{0}: {1}")]
    Windows(&'static str, #[source] windows::core::Error),
    #[cfg(windows)]
    #[error("the AAC encoder made {made} of the clip's {needed} frames")]
    Short { made: usize, needed: usize },
    #[cfg(not(windows))]
    #[error("sound is only encoded on Windows")]
    Unsupported,
}

#[cfg(windows)]
pub(crate) fn encode(clip: &PcmClip) -> Result<AudioClip, EncodeError> {
    aac::encode_clip(clip)
}

#[cfg(not(windows))]
pub(crate) fn encode(_clip: &PcmClip) -> Result<AudioClip, EncodeError> {
    Err(EncodeError::Unsupported)
}

#[cfg(not(windows))]
pub(crate) struct Capture;

#[cfg(not(windows))]
pub(crate) fn start(_live: &std::sync::Arc<crate::replay::Live>) -> Option<Capture> {
    None
}
