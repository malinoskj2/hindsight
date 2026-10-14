//! One capture session: a frame per tick of the clock, from the capture through NVENC into the replay buffer.

use crate::{
    config::{Codec, Config, Memory},
    gpu::{
        capture::{self, CaptureError, CopyTarget, EncoderDevice, Frame, Surface},
        cuda_output::CudaRing,
        device::{Device, DeviceError},
        nvenc::{self, Encoded, Encoder, NvencError, Payload, Settings},
        pacer::{FrameIndex, Pacer},
        priority, vram,
    },
    log,
    replay::{self, DiskStore, Live, Packet, Picture, Stream},
    service::{Ended, Target},
    storage, sys,
};
use anyhow::{Context, Result};
use std::{
    num::NonZeroU32,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// A window capture that shows nothing this long cannot see the window (exclusive full screen).
const FIRST_FRAME_TIMEOUT_SECONDS: u64 = 2;

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

fn prepare_thread() -> Option<priority::MultimediaThread> {
    let multimedia = match priority::join_multimedia_class() {
        Ok(multimedia) => Some(multimedia),
        Err(error) => {
            raise_thread_instead(&error);
            None
        }
    };
    if let Err(error) = priority::opt_out_of_throttling() {
        log::warning(format!("Power throttling · not turned off ({error})"));
    }
    match priority::raise_gpu_class() {
        Ok(()) => log::info("GPU priority · high"),
        Err(error) => log::warning(format!("GPU priority · not raised ({error})")),
    }
    multimedia
}

fn raise_thread_instead(error: &windows::core::Error) {
    log::warning(format!(
        "Capture thread · no multimedia class ({error}) · high priority instead"
    ));
    if let Err(error) = priority::raise_thread() {
        log::warning(format!("Capture thread · priority not raised ({error})"));
    }
}

fn run(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    if let Some(processor) = sys::thread::ideal_processor() {
        live.note_capture_processor(processor);
    }
    let device = open_device(config)?;
    let frame_rate = NonZeroU32::new(config.frame_rate).context("the frame rate is zero")?;
    let mut pacer = Pacer::new(frame_rate).context("create the frame timer")?;
    // Frame `i` is this instant plus `i` periods; the audio is lined up against it.
    live.set_origin(sys::clock::now());
    let mut session = Session::open(config, device, pacer.period(), target, live)?;
    let settings = session.encoder.settings();
    live.set_stream(Stream {
        codec: settings.codec,
        width: settings.width,
        height: settings.height,
        frame_rate: settings.frame_rate,
        matrix: settings.matrix,
    });
    let mut keyframes = Keyframes::new(settings.codec);
    let mut output = |encoded: Encoded| push_frame(live, &mut keyframes, encoded);

    let ended = loop {
        if stop.load(Ordering::Relaxed) {
            break Ended::Stopped;
        }
        let index = pacer.wait().context("wait for the frame timer")?;
        if let Some(ended) = session.tick(index, &mut output)? {
            break ended;
        }
    };
    session.encoder.finish(&mut output)?;
    Ok(ended)
}

fn push_frame(live: &Live, keyframes: &mut Keyframes, encoded: Encoded) {
    let keyframe = keyframes.classify(&encoded);
    match encoded.data {
        Payload::Memory(data) => live.push(
            Packet {
                index: encoded.index.get(),
                keyframe,
                data,
            },
            encoded.picture,
        ),
        Payload::Stored { slot, .. } => {
            live.push_stored(encoded.index.get(), keyframe, slot, encoded.picture);
        }
    }
}

/// The encoder owns what it runs on and what it reads in place, except a CUDA context the NvFBC
/// driver opened, which lives with the capture's session; so it is declared first and dropped
/// before the capture.
struct Session {
    encoder: Encoder,
    capture: capture::Capture,
    device: Device,
    surfaces: SurfaceDelivery,
    first_frame_deadline: Option<FrameIndex>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SurfaceDelivery {
    /// NVENC reads the capture's surfaces where they are.
    InPlace,
    /// Each frame is copied into the encoder's own input first.
    Copied,
}

impl Session {
    fn open(
        config: &Config,
        device: Device,
        period: Duration,
        target: &Target,
        live: &Live,
    ) -> Result<Self> {
        let mut capture = capture::start(config.backend, &device, period, target, config.cursor)?;
        let (width, height) = encodable(capture.size());
        let mut on = capture.encoder_device();
        choose_output(config, &mut on, live);
        let encoder = Encoder::open(
            &device.d3d,
            on,
            Settings {
                codec: config.codec,
                width,
                height,
                frame_rate: config.frame_rate,
                bitrate_bps: config.bitrate(),
                matrix: capture.colour_matrix(),
            },
        )?;
        // Surfaces are read in place only when nothing needs cropping.
        if capture.size() == (width, height) {
            capture.hand_over_surfaces();
        }
        let first_frame_deadline = capture
            .captures_window()
            .then(|| FrameIndex::new(u64::from(config.frame_rate) * FIRST_FRAME_TIMEOUT_SECONDS));
        Ok(Self {
            encoder,
            capture,
            device,
            surfaces: SurfaceDelivery::InPlace,
            first_frame_deadline,
        })
    }

    fn tick(
        &mut self,
        index: FrameIndex,
        output: &mut impl FnMut(Encoded),
    ) -> Result<Option<Ended>> {
        self.encoder.collect(output)?;
        self.encoder.reserve(output)?;
        if let Some(surface) = self.capture.next_surface() {
            self.encoder.release_surface(surface, output)?;
        }
        let encoder = &self.encoder;
        self.capture
            .release_surfaces(&|surface| encoder.holds_surface(surface));
        match fill_input(&self.device, &mut self.capture, &self.encoder)? {
            Input::Ready => self.encoder.submit(index, Picture::Fresh)?,
            Input::Repeat => repeat_latest(&self.device, &mut self.encoder, index, output)?,
            Input::InSurface { surface, handle } => self.submit_surface(index, surface, &handle)?,
            Input::Empty
                if self
                    .first_frame_deadline
                    .is_some_and(|deadline| index >= deadline) =>
            {
                return Ok(Some(Ended::NoFrames));
            }
            Input::Empty => {}
            Input::Ended(ended) => return Ok(Some(ended)),
        }
        Ok(None)
    }

    fn submit_surface(
        &mut self,
        index: FrameIndex,
        surface: usize,
        handle: &Surface,
    ) -> Result<(), NvencError> {
        if self.surfaces == SurfaceDelivery::InPlace
            && !self.encoder.has_surface(surface)
            && let Err(error) = self.encoder.register_surface(surface, handle)
        {
            if !matches!(handle, Surface::Texture(_)) || self.encoder.input().is_none() {
                return Err(error);
            }
            log::warning(format!(
                "NVENC · cannot read grabs in place ({error}) · copying each frame"
            ));
            self.surfaces = SurfaceDelivery::Copied;
        }
        if self.surfaces == SurfaceDelivery::Copied
            && let (Surface::Texture(texture), Some(input)) = (handle, self.encoder.input())
        {
            let Settings { width, height, .. } = self.encoder.settings();
            self.device.copy_region(input, texture, width, height);
            return self.encoder.submit(index, Picture::Fresh);
        }
        self.encoder.submit_surface(index, surface)
    }
}

fn repeat_latest(
    device: &Device,
    encoder: &mut Encoder,
    index: FrameIndex,
    output: &mut impl FnMut(Encoded),
) -> Result<(), NvencError> {
    if encoder.repeat(index)? {
        return Ok(());
    }
    match (encoder.input(), encoder.latest_texture()) {
        (Some(input), Some(latest)) => {
            device.copy(input, latest);
            encoder.submit(index, Picture::Repeated)
        }
        _ => encoder.repeat_once_free(index, output),
    }
}

/// Where the replay buffer goes: a store for the replay buffer, or a ring in the CUDA session
/// `on` for the encoder to write into.
fn choose_output(config: &Config, on: &mut EncoderDevice, live: &Live) {
    match config.buffer_memory {
        Memory::Ram => return,
        Memory::Disk => {
            keep_on_disk(config, live);
            return;
        }
        Memory::Vram => {}
    }
    let bytes = config.estimated_buffer_bytes();
    if let EncoderDevice::Cuda { hold, ring } = on
        && let Some(device) = hold.primary()
    {
        match CudaRing::create(device, bytes, nvenc::STAGING_LAYOUT) {
            Ok(created) => {
                log::info(created.description());
                *ring = Some(created);
                return;
            }
            Err(error) => log::warning(format!(
                "Buffer · not in CUDA memory ({}) · trying Direct3D",
                log::chain(&error)
            )),
        }
    }
    match vram::VideoMemory::create(bytes) {
        Ok(store) => {
            log::info(store.description());
            live.set_store(Box::new(store));
        }
        Err(error) => log::warning(format!(
            "Buffer · not in GPU memory ({}) · kept in RAM",
            log::chain(&error)
        )),
    }
}

fn keep_on_disk(config: &Config, live: &Live) {
    match DiskStore::create(&storage::buffer_file(), config.estimated_buffer_bytes()) {
        Ok(store) => {
            log::info(store.description());
            live.set_store(Box::new(store));
        }
        Err(error) => log::warning(format!(
            "Buffer · not on disk ({}) · kept in RAM",
            log::chain(&error)
        )),
    }
}

fn open_device(config: &Config) -> Result<Device> {
    let device = Device::create()?;
    log::info(format!(
        "Capture · {} on {}",
        config.backend, device.adapter
    ));
    if let Err(error) = device.raise_priority() {
        log::warning(format!(
            "GPU thread priority · not raised ({error}) · run as administrator"
        ));
    }
    Ok(device)
}

/// 4:2:0 needs even dimensions.
const fn encodable((width, height): (u32, u32)) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Parses the bitstream only until the driver has shown that it flags keyframes.
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

    fn classify(&mut self, encoded: &Encoded) -> bool {
        let idr = encoded.is_idr();
        self.driver_flags_keyframes |= idr;
        idr || (!self.driver_flags_keyframes && replay::is_keyframe(self.codec, encoded.head()))
    }
}

enum Input {
    Ready,
    Repeat,
    InSurface { surface: usize, handle: Surface },
    Empty,
    Ended(Ended),
}

fn fill_input(
    device: &Device,
    capture: &mut capture::Capture,
    encoder: &Encoder,
) -> Result<Input, CaptureError> {
    let Settings { width, height, .. } = encoder.settings();
    let target = encoder.input().map(|texture| CopyTarget {
        device,
        texture,
        width,
        height,
    });
    let frame = match capture.take(target) {
        Ok(frame) => frame,
        Err(CaptureError::Closed) => return Ok(Input::Ended(Ended::Closed)),
        Err(error) => return Err(error),
    };
    Ok(match frame {
        Frame::Fresh => Input::Ready,
        Frame::InSurface { index, surface } => Input::InSurface {
            surface: index,
            handle: surface,
        },
        Frame::Unchanged if encoder.has_latest() => Input::Repeat,
        Frame::Unchanged => Input::Empty,
        Frame::Resized => Input::Ended(Ended::Resized),
    })
}

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
