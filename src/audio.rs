//! System sound and the microphone: WASAPI capture on its own thread, stamped on the video clock,
//! and mixed and encoded to AAC-LC when a clip is saved.

#[cfg(windows)]
#[expect(unsafe_code, reason = "calls Media Foundation's AAC encoder")]
mod aac;
mod pcm;
#[cfg(windows)]
mod recorder;
#[cfg(test)]
pub(crate) mod testing;
mod timeline;
#[cfg(windows)]
#[expect(unsafe_code, reason = "calls WASAPI and COM")]
mod wasapi;

#[cfg(windows)]
pub(crate) use recorder::{Capture, start};

use crate::replay::{AudioClip, PcmClip, Source};
use std::num::{NonZeroU16, NonZeroU32};
use thiserror::Error;

/// Windows resamples other rates to this.
#[cfg_attr(
    all(not(windows), test),
    expect(dead_code, reason = "only the Windows capture and encoder use it")
)]
const SAMPLE_RATE: NonZeroU32 = NonZeroU32::new(48_000).unwrap();
#[cfg_attr(
    all(not(windows), test),
    expect(dead_code, reason = "only the Windows capture and encoder use it")
)]
const CHANNELS: NonZeroU16 = NonZeroU16::new(2).unwrap();

/// The sources a session records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sources {
    pub(crate) system: bool,
    pub(crate) microphone: bool,
}

impl Sources {
    pub(crate) const fn any(self) -> bool {
        self.system || self.microphone
    }

    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture uses it")
    )]
    fn iter(self) -> impl Iterator<Item = Source> {
        [
            (self.system, Source::System),
            (self.microphone, Source::Microphone),
        ]
        .into_iter()
        .filter_map(|(recorded, source)| recorded.then_some(source))
    }
}

#[derive(Debug, Error)]
pub(crate) enum EncodeError {
    #[cfg(windows)]
    #[error("could not {0}")]
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

/// Never constructed off Windows.
#[cfg(not(windows))]
pub(crate) enum Capture {}

#[cfg(not(windows))]
impl Capture {
    pub(crate) fn has_stopped(&self) -> bool {
        match *self {}
    }
}

#[cfg(not(windows))]
pub(crate) fn start(
    _live: &std::sync::Arc<crate::replay::Live>,
    _sources: Sources,
) -> Option<Capture> {
    None
}
