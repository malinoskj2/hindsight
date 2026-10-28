//! System sound and the microphone: WASAPI capture on its own thread, stamped on the video clock,
//! and mixed and encoded to AAC-LC when a clip is saved.

#[cfg(windows)]
#[expect(unsafe_code, reason = "calls Media Foundation's AAC encoder")]
mod aac;
mod choice;
#[cfg(windows)]
mod listing;
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
pub(crate) use choice::{Entry, entries};
#[cfg(windows)]
pub(crate) use listing::DeviceList;
#[cfg(windows)]
pub(crate) use recorder::{Capture, start};

use crate::{
    config::{Config, DeviceId},
    replay::{AudioClip, PcmClip, PerSource, Source},
};
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceChoice {
    Default,
    Chosen(DeviceId),
}

impl DeviceChoice {
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture uses it")
    )]
    pub(crate) const fn chosen(&self) -> Option<&DeviceId> {
        match self {
            Self::Default => None,
            Self::Chosen(device) => Some(device),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sources(pub(crate) PerSource<Option<DeviceChoice>>);

impl Sources {
    pub(crate) fn of(config: &Config) -> Self {
        let choice = |recorded: bool, device: &Option<DeviceId>| {
            recorded.then(|| {
                device
                    .clone()
                    .map_or(DeviceChoice::Default, DeviceChoice::Chosen)
            })
        };
        Self(PerSource {
            system: choice(config.audio, &config.system_device),
            microphone: choice(config.microphone, &config.microphone_device),
        })
    }

    pub(crate) fn any(&self) -> bool {
        self.recorded().next().is_some()
    }

    pub(crate) fn recorded(&self) -> impl Iterator<Item = (Source, &DeviceChoice)> {
        self.0
            .iter()
            .filter_map(|(source, device)| device.as_ref().map(|device| (source, device)))
    }
}

/// The sources recorded from Windows' default device because the device chosen for them in
/// Settings is missing.
pub(crate) type StandIns = PerSource<bool>;

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
pub(crate) fn encode(clip: &PcmClip) -> Result<Vec<AudioClip>, EncodeError> {
    aac::encode_clip(clip)
}

#[cfg(not(windows))]
pub(crate) fn encode(_clip: &PcmClip) -> Result<Vec<AudioClip>, EncodeError> {
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

    pub(crate) fn stand_ins(&self) -> StandIns {
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
