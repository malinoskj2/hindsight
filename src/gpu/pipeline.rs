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
    config::{Codec, Config},
    log,
    replay::{self, Live, Packet, Stream},
    service::{Ended, Target},
    sys,
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

/// Raises the thread's scheduling priorities for the session, and returns its multimedia-class
/// membership, which lasts as long as the session.
fn prepare_thread() -> Option<priority::MultimediaThread> {
    let multimedia = match priority::join_multimedia_class() {
        Ok(multimedia) => Some(multimedia),
        Err(error) => {
            raise_thread_instead(&error);
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

/// Falls back to a high thread priority when joining the multimedia class failed with `error`.
fn raise_thread_instead(error: &windows::core::Error) {
    log::warning(format!(
        "The capture thread could not join the multimedia scheduling class ({error}); using a high thread priority instead"
    ));
    if let Err(error) = priority::raise_thread() {
        log::warning(format!(
            "The capture thread priority was not raised: {error}"
        ));
    }
}

/// Opens the device, the capture and the encoder, then encodes one frame per tick of the frame
/// clock until the session ends.
fn run(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    let device = open_device(config)?;
    let mut pacer = Pacer::new(config.frame_rate).context("create the frame timer")?;
    // The pacer's first tick is due at once: frame `i` stands for this time plus `i` periods,
    // which is how the sound is lined up with the video.
    live.set_origin(sys::clock::now());
    let mut capture = capture::start(
        config.backend,
        &device,
        pacer.period(),
        target,
        config.cursor,
    )?;
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
    // NVENC reads a capture's own surfaces in place where it can, which saves copying every
    // frame into the encoder's input.
    if capture.size() == (width, height) {
        capture.hand_over_surfaces();
    }
    let mut copy_surfaces = false;
    let settings = encoder.settings();
    live.set_stream(Stream {
        codec: settings.codec,
        width: settings.width,
        height: settings.height,
        frame_rate: settings.frame_rate,
    });

    let mut keyframes = Keyframes::new(settings.codec);
    let mut output = |encoded: Encoded| {
        let keyframe = keyframes.classify(&encoded);
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
        if let Some(surface) = capture.next_surface() {
            encoder.release_surface(surface, &mut output)?;
        }
        capture.release_surfaces(&|surface| encoder.holds_surface(surface));
        match fill_input(&device, &mut *capture, &encoder, (width, height))? {
            Input::Ready { fresh } => encoder.submit(index, fresh)?,
            Input::InSurface(surface) => submit_surface(
                &device,
                &*capture,
                &mut encoder,
                index,
                surface,
                &mut copy_surfaces,
            )?,
            Input::Empty if first_frame_deadline.is_some_and(|deadline| index >= deadline) => {
                break Ended::NoFrames;
            }
            Input::Empty => {}
            Input::Ended(ended) => break ended,
        }
    };

    encoder.finish(&mut output)?;
    Ok(ended)
}

/// Submits the new picture in the capture's surface `surface` as frame `index`: in place,
/// registering the surface with NVENC the first time it comes, or copied into the encoder's
/// input once NVENC has refused a surface (`copy_instead`).
fn submit_surface(
    device: &Device,
    capture: &dyn capture::Source,
    encoder: &mut Encoder,
    index: u64,
    surface: usize,
    copy_instead: &mut bool,
) -> Result<(), NvencError> {
    let texture = capture
        .surface(surface)
        .expect("a capture reports only surfaces of its own");
    if !*copy_instead
        && !encoder.has_surface(surface)
        && let Err(error) = encoder.register_surface(surface, &texture)
    {
        log::warning(format!(
            "NVENC could not read the captured surfaces directly ({error}); copying each frame instead"
        ));
        *copy_instead = true;
    }
    if *copy_instead {
        let Settings { width, height, .. } = encoder.settings();
        device.copy_region(encoder.input(), &texture, width, height);
        return encoder.submit(index, true);
    }
    encoder.submit_surface(index, surface)
}

/// Creates the device to capture and encode on, logs it, and raises its GPU thread priority
/// where Windows allows.
fn open_device(config: &Config) -> Result<Device> {
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
    Ok(device)
}

/// NVENC encodes 4:2:0 video, whose dimensions must be even; an odd row or column is dropped.
const fn encodable((width, height): (u32, u32)) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Decides which encoded frames are keyframes.
///
/// NVENC reports the picture type it chose. Parsing the bitstream instead costs about 100 µs
/// per frame for H.264 and HEVC, most of the CPU budget, so it is used only until the driver
/// has shown that it flags keyframes, which the first one does within a second.
struct Keyframes {
    codec: Codec,
    driver_flags_keyframes: bool,
}

impl Keyframes {
    const fn new(codec: Codec) -> Self {
        Self {
            codec,
            driver_flags_keyframes: false,
        }
    }

    /// Whether `encoded` is a keyframe, which a clip can start with.
    fn classify(&mut self, encoded: &Encoded) -> bool {
        let idr = encoded.is_idr();
        self.driver_flags_keyframes |= idr;
        idr || (!self.driver_flags_keyframes && replay::is_keyframe(self.codec, &encoded.data))
    }
}

/// What a tick left in the encoder's input.
enum Input {
    /// A picture to submit, which is `fresh` unless it repeats the previous one.
    Ready { fresh: bool },
    /// A new picture to submit from the capture's surface with this index.
    InSurface(usize),
    /// Nothing has been captured yet; there is no picture to repeat.
    Empty,
    /// The capture ended the session.
    Ended(Ended),
}

/// Fills the input of the slot [`Encoder::reserve`] freed with the newest captured picture of
/// `width` × `height`, or with the previous one when the screen did not change.
fn fill_input(
    device: &Device,
    capture: &mut dyn capture::Source,
    encoder: &Encoder,
    (width, height): (u32, u32),
) -> Result<Input, CaptureError> {
    let input = encoder.input();
    let frame = match capture.take(device, input, width, height) {
        Ok(frame) => frame,
        Err(CaptureError::Closed) => return Ok(Input::Ended(Ended::Closed)),
        Err(error) => return Err(error),
    };
    Ok(match frame {
        Frame::Fresh => Input::Ready { fresh: true },
        Frame::InSurface(surface) => Input::InSurface(surface),
        Frame::Unchanged => match encoder.latest() {
            Some(latest) => {
                device.copy(input, latest);
                Input::Ready { fresh: false }
            }
            None => Input::Empty,
        },
        Frame::Resized => Input::Ended(Ended::Resized),
    })
}

/// Whether `error` comes from the machine or the settings rather than the moment.
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
