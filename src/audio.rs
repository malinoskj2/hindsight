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

/// How a recorded source is captured, for the Status tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
#[cfg_attr(
    all(not(windows), test),
    expect(
        dead_code,
        reason = "only the Windows audio thread finds a source silent"
    )
)]
pub(crate) enum InputState {
    /// From the device chosen, or from Windows' default when none is. Also while a device opens.
    #[default]
    Captured,
    /// From Windows' default, because the device chosen in Settings is missing.
    StandIn,
    /// Silent, as there is no device.
    NoDevice,
    /// Silent, as Windows' privacy settings deny desktop apps the device.
    Denied,
    /// Silent, as the device cannot be captured, for a reason the log gives.
    Unavailable,
}

pub(crate) type InputStates = PerSource<InputState>;

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

    pub(crate) fn input_states(&self) -> InputStates {
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
