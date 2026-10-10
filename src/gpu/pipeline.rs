//! One capture session: on every tick of the frame clock, the newest picture from the chosen
//! capture backend (or, when the screen did not change, the previous one) is copied into a free
//! encoder slot and submitted, and whatever NVENC has finished goes into the replay buffer.

use super::{
    capture::{self, CaptureError, Frame},
    device::{Device, DeviceError},
    nvenc::{Encoded, Encoder, NvencError, Settings},
    pacer::Pacer,
    priority,
};
use crate::{
    config::Config,
    log,
    replay::{self, Live, Packet, Stream},
    service::{Ended, Target},
};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// How long a window capture may go without a first frame before it is judged unable to see
/// the window (exclusive full screen, which DWM does not compose), in seconds.
const FIRST_FRAME_TIMEOUT_SECONDS: u64 = 2;

/// Records `target` into `live` until `stop` is set, the target changes size or goes away, or
/// something fails.
pub(crate) fn record(
    config: &Config,
    target: &Target,
    live: &Live,
    stop: &AtomicBool,
) -> Result<Ended> {
    let _scheduling = prepare_thread();
    match run(config, target, live, stop) {
        Err(error) if is_permanent(&error) => Ok(Ended::Unsupported(format!("{error:#}"))),
        result => result,
    }
}

/// Failures that come from the machine or the settings rather than the moment.
fn is_permanent(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<DeviceError>(),
        Some(DeviceError::NoNvidiaAdapter)
    ) || matches!(
        error.downcast_ref::<CaptureError>(),
        Some(CaptureError::Unsupported(_))
    ) || matches!(
        error.downcast_ref::<NvencError>(),
        Some(
            NvencError::Library
                | NvencError::DriverTooOld(..)
                | NvencError::VersionRejected
                | NvencError::Unsupported(_)
                | NvencError::TooLarge { .. }
        )
    )
}

fn run(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    let device = Device::create()?;
    log::info(format!(
        "Capturing on {} with {}",
        device.adapter, config.backend
    ));
    if let Err(error) = device.raise_priority() {
        log::warning(format!(
            "GPU thread priority was not raised ({error}); run as administrator for the highest priority"
        ));
    }

    let mut pacer = Pacer::new(config.frame_rate).context("create the frame timer")?;
    let mut capture = capture::start(config.backend, &device, pacer.period(), target)?;
    let (width, height) = encodable(capture.size());
    let mut encoder = Encoder::open(
        &device.d3d,
        Settings {
            codec: config.codec,
            width,
            height,
            frame_rate: config.frame_rate,
            bitrate: config.bitrate(),
        },
    )?;
    let settings = encoder.settings();
    live.set_stream(Stream {
        codec: settings.codec,
        width: settings.width,
        height: settings.height,
        frame_rate: settings.frame_rate,
    });

    // NVENC reports the picture type it chose. Parsing the bitstream instead costs about
    // 100 µs per frame for H.264 and HEVC, most of the CPU budget, so it is used only until
    // the driver has shown that it flags keyframes, which the first one does within a second.
    let mut driver_flags_keyframes = false;
    let mut output = |encoded: Encoded| {
        let keyframe = if encoded.is_idr() {
            driver_flags_keyframes = true;
            true
        } else if driver_flags_keyframes {
            false
        } else {
            replay::is_keyframe(settings.codec, &encoded.data)
        };
        live.push(
            Packet {
                index: encoded.index,
                keyframe,
                data: encoded.data,
            },
            encoded.fresh,
        );
    };

    let first_frame_deadline = capture
        .captures_window()
        .then(|| u64::from(config.frame_rate) * FIRST_FRAME_TIMEOUT_SECONDS);
    let ended = loop {
        if stop.load(Ordering::Relaxed) {
            break Ended::Stopped;
        }
        let index = pacer.wait().context("wait for the frame timer")?;
        encoder.collect(&mut output)?;

        encoder.reserve(&mut output)?;
        let input = encoder.input();
        let fresh = match capture.take(&device, input, width, height) {
            Ok(Frame::Fresh) => true,
            Ok(Frame::Unchanged) => {
                let Some(latest) = encoder.latest() else {
                    // Nothing has been captured yet; there is no picture to repeat.
                    if first_frame_deadline.is_some_and(|deadline| index >= deadline) {
                        break Ended::NoFrames;
                    }
                    continue;
                };
                device.copy(input, latest);
                false
            }
            Ok(Frame::Resized) => break Ended::Resized,
            Err(CaptureError::Closed) => break Ended::Closed,
            Err(error) => return Err(error.into()),
        };
        encoder.submit(index, fresh)?;
    };

    encoder.finish(&mut output)?;
    Ok(ended)
}

/// NVENC encodes 4:2:0 video, whose dimensions must be even; an odd row or column is dropped.
const fn encodable((width, height): (u32, u32)) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Returns the thread's multimedia-class membership, which lasts as long as the session.
fn prepare_thread() -> Option<priority::MultimediaThread> {
    let multimedia = match priority::join_multimedia_class() {
        Ok(multimedia) => Some(multimedia),
        Err(error) => {
            log::warning(format!(
                "The capture thread could not join the multimedia scheduling class ({error}); using a high thread priority instead"
            ));
            if let Err(error) = priority::raise_thread() {
                log::warning(format!(
                    "The capture thread priority was not raised: {error}"
                ));
            }
            None
        }
    };
    if let Err(error) = priority::opt_out_of_throttling() {
        log::warning(format!(
            "Windows power throttling could not be turned off: {error}"
        ));
    }
    if priority::raise_gpu_class() {
        log::info("GPU scheduling priority: high");
    } else {
        log::warning("The GPU scheduling priority could not be raised");
    }
    multimedia
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_dimensions_are_rounded_down_to_even() {
        assert_eq!(encodable((1920, 1080)), (1920, 1080));
        assert_eq!(encodable((1366, 767)), (1366, 766));
        assert_eq!(encodable((3441, 1441)), (3440, 1440));
    }
}
