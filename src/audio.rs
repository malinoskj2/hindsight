//! System sound for the clips: what the default output device plays, captured through WASAPI
//! loopback on a thread of its own, encoded to AAC-LC at 160 kb/s and handed to the session's
//! buffer numbered on the clock the video frames use (the replay module's `audio` describes
//! that clock).
//!
//! The sound never holds up the video. It runs on its own thread, which takes the buffer's lock
//! only to append a frame; a device that goes away or stops being the default is reopened by
//! that thread alone, with silence filling the time in between; and a session whose sound
//! cannot be captured at all records its video without it.
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

/// The sample rate the sound is encoded at, which Windows converts other rates to.
const SAMPLE_RATE: u32 = 48_000;
/// Stereo: Windows mixes more channels down, and mono is played on both sides.
const CHANNELS: u16 = 2;

/// Elsewhere there is no sound to capture.
#[cfg(not(windows))]
pub(crate) struct Capture;

/// Elsewhere there is no sound to capture, so sessions record video only.
#[cfg(not(windows))]
pub(crate) fn start(_live: &std::sync::Arc<crate::replay::Live>) -> Option<Capture> {
    None
}
