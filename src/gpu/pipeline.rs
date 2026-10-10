//! One capture session: on every tick of the frame clock, the newest captured picture (or, when
//! the screen did not change, the previous one) is copied into a free encoder slot and
//! submitted, and whatever NVENC has finished goes into the replay buffer.

use super::{
    capture::{self, Capture, CaptureError, Frame},
    device::{Device, DeviceError},
    nvenc::{Encoded, Encoder, NvencError, Settings},
    pacer::Pacer,
    priority,
};
use crate::{
    config::Config,
    log,
    replay::{self, Live, Packet, Stream},
    service::Ended,
};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// Records the primary monitor into `live` until `stop` is set, the display changes size or
/// something fails.
pub(crate) fn record(config: &Config, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    prepare_thread();
    match run(config, live, stop) {
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
        Some(CaptureError::Unsupported)
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

fn run(config: &Config, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    let device = Device::create()?;
    log::info(format!("Capturing on {}", device.adapter));
    if let Err(error) = device.raise_priority() {
        log::warning(format!(
            "GPU thread priority was not raised ({error}); run as administrator for the highest priority"
        ));
    }

    let mut pacer = Pacer::new(config.frame_rate).context("create the frame timer")?;
    let capture = Capture::primary_monitor(&device, pacer.period())?;
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

    let mut output = |encoded: Encoded| {
        // Parsing the bitstream is the authority for all three codecs, whatever the driver
        // reports; the driver's IDR flag is a cheap fast path that skips the parse on most
        // keyframes.
        let keyframe = encoded.is_idr() || replay::is_keyframe(settings.codec, &encoded.data);
        live.push(
            Packet {
                index: encoded.index,
                keyframe,
                data: encoded.data,
            },
            encoded.fresh,
        );
    };

    let ended = loop {
        if stop.load(Ordering::Relaxed) {
            break Ended::Stopped;
        }
        let index = pacer.wait().context("wait for the frame timer")?;
        encoder.collect(&mut output)?;

        let target = encoder.input(&mut output)?.clone();
        let fresh = match capture.take(&device, &target, width, height)? {
            Frame::Fresh => true,
            Frame::Unchanged => match encoder.latest() {
                Some(latest) => {
                    device.copy(&target, latest);
                    false
                }
                // Nothing has been captured yet; there is no picture to repeat.
                None => continue,
            },
            Frame::Resized => break Ended::Resized,
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

fn prepare_thread() {
    capture::join_apartment();
    if let Err(error) = priority::raise_thread() {
        log::warning(format!(
            "The capture thread priority was not raised: {error}"
        ));
    }
    if let Err(error) = priority::opt_out_of_throttling() {
        log::warning(format!(
            "Windows power throttling could not be turned off: {error}"
        ));
    }
    match priority::raise_gpu_class() {
        Some(class) => log::info(format!("GPU scheduling priority: {class}")),
        None => log::warning("The GPU scheduling priority could not be raised"),
    }
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
