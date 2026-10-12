//! One capture session: on every tick of the frame clock, the newest picture from the chosen
//! capture backend (or, when the screen did not change, the previous one) is copied into a free
//! encoder slot and submitted, and whatever NVENC has finished goes into the replay buffer.

use super::{
    capture::{self, CaptureError, EncoderDevice, Frame, Surface},
    cuda_output::CudaRing,
    device::{Device, DeviceError},
    nvenc::{Encoded, Encoder, NvencError, Output, Payload, Settings},
    pacer::Pacer,
    priority, vram,
};
use crate::{
    config::{Codec, Config, Memory},
    log,
    replay::{self, DiskStore, Live, Packet, Stream},
    service::{Ended, Target},
    storage, sys,
};
use anyhow::{Context, Result};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

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

/// Opens the device, the frame clock, the capture and the encoder, then encodes one frame per
/// tick of the clock until the session ends.
fn run(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    sys::thread::note_capture_thread();
    let device = open_device(config)?;
    let mut pacer = Pacer::new(config.frame_rate).context("create the frame timer")?;
    // The pacer's first tick is due at once: frame `i` stands for this time plus `i` periods,
    // which is how the sound is lined up with the video.
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

/// Puts a finished frame into the buffer, wherever the encoder left its bitstream: in the
/// packet, or in a slot of the ring in video memory.
fn push_frame(live: &Live, keyframes: &mut Keyframes, encoded: Encoded) {
    let keyframe = keyframes.classify(&encoded);
    match encoded.data {
        Payload::Memory(data) => live.push(
            Packet {
                index: encoded.index,
                keyframe,
                data,
            },
            encoded.fresh,
        ),
        Payload::Stored { slot, .. } => {
            live.push_stored(encoded.index, keyframe, slot, encoded.fresh);
        }
    }
}

/// The capture and the encoder of one session, and what a tick needs to move a picture from
/// the one to the other. The encoder is declared first, so that it is dropped before the
/// capture, whose surfaces (and CUDA context) it reads, and before the device.
struct Session {
    encoder: Encoder,
    capture: Box<dyn capture::Source>,
    device: Device,
    /// NVENC refused to read the capture's surfaces in place, so each new picture is copied
    /// into the encoder's input instead.
    copy_surfaces: bool,
    /// The tick by which a window capture must have delivered a first frame, or the window is
    /// judged invisible to it.
    first_frame_deadline: Option<u64>,
}

impl Session {
    /// Starts capturing `target` on `device`, paced to `period`, and opens the encoder on the
    /// device the capture's surfaces are reachable from, sending its frames where `config`
    /// asks.
    fn open(
        config: &Config,
        device: Device,
        period: Duration,
        target: &Target,
        live: &Live,
    ) -> Result<Self> {
        let mut capture = capture::start(config.backend, &device, period, target, config.cursor)?;
        let (width, height) = encodable(capture.size());
        let on = capture.encoder_device();
        let output = choose_output(config, &on, live);
        let encoder = Encoder::open(
            &device.d3d,
            &on,
            Settings {
                codec: config.codec,
                width,
                height,
                frame_rate: config.frame_rate,
                bitrate: config.bitrate(),
                matrix: capture.colour_matrix(),
            },
            output,
        )?;
        // NVENC reads a capture's own surfaces in place where it can, which saves copying
        // every frame into the encoder's input.
        if capture.size() == (width, height) {
            capture.hand_over_surfaces();
        }
        let first_frame_deadline = capture
            .captures_window()
            .then(|| u64::from(config.frame_rate) * FIRST_FRAME_TIMEOUT_SECONDS);
        Ok(Self {
            encoder,
            capture,
            device,
            copy_surfaces: false,
            first_frame_deadline,
        })
    }

    /// Encodes frame `index`: hands `output` what NVENC has finished, frees a slot, and submits
    /// the newest picture, new or repeated, from wherever the capture left it. Reports how the
    /// session ended when the capture ended it, or when a window capture has shown nothing by
    /// its deadline.
    fn tick(&mut self, index: u64, output: &mut impl FnMut(Encoded)) -> Result<Option<Ended>> {
        let Self {
            encoder,
            capture,
            device,
            copy_surfaces,
            first_frame_deadline,
        } = self;
        encoder.collect(output)?;
        encoder.reserve(output)?;
        if let Some(surface) = capture.next_surface() {
            encoder.release_surface(surface, output)?;
        }
        capture.release_surfaces(&|surface| encoder.holds_surface(surface));
        match fill_input(device, &mut **capture, encoder)? {
            Input::Ready => encoder.submit(index, true)?,
            Input::Repeat => repeat_latest(device, encoder, index, output)?,
            Input::InSurface(surface) => {
                submit_surface(device, &**capture, encoder, index, surface, copy_surfaces)?;
            }
            Input::Empty if first_frame_deadline.is_some_and(|deadline| index >= deadline) => {
                return Ok(Some(Ended::NoFrames));
            }
            Input::Empty => {}
            Input::Ended(ended) => return Ok(Some(ended)),
        }
        Ok(None)
    }
}

/// Submits the new picture in the capture's surface `surface` as frame `index`: in place,
/// registering the surface with NVENC the first time it comes, or copied into the encoder's
/// input once NVENC has refused a surface (`copy_instead`). A surface in CUDA memory or on a
/// Direct3D 9 device cannot be copied, so a refusal of one is an error.
fn submit_surface(
    device: &Device,
    capture: &dyn capture::Source,
    encoder: &mut Encoder,
    index: u64,
    surface: usize,
    copy_instead: &mut bool,
) -> Result<(), NvencError> {
    let handle = capture
        .surface(surface)
        .expect("a capture reports only surfaces of its own");
    if !*copy_instead
        && !encoder.has_surface(surface)
        && let Err(error) = encoder.register_surface(surface, &handle)
    {
        if !matches!(handle, Surface::Texture(_)) || encoder.input().is_none() {
            return Err(error);
        }
        log::warning(format!(
            "NVENC could not read the captured surfaces directly ({error}); copying each frame instead"
        ));
        *copy_instead = true;
    }
    if *copy_instead && let (Surface::Texture(texture), Some(input)) = (&handle, encoder.input()) {
        let Settings { width, height, .. } = encoder.settings();
        device.copy_region(input, texture, width, height);
        return encoder.submit(index, true);
    }
    encoder.submit_surface(index, surface)
}

/// Submits the newest picture again as frame `index`, since the screen did not change: NVENC
/// reads it where it is, or, while it is still reading it for the frame before, from a copy in
/// the encoder's input; a picture in CUDA memory or on a Direct3D 9 device, which has no copy,
/// is read again once NVENC is done with it.
fn repeat_latest(
    device: &Device,
    encoder: &mut Encoder,
    index: u64,
    output: &mut impl FnMut(Encoded),
) -> Result<(), NvencError> {
    if encoder.repeat(index)? {
        return Ok(());
    }
    match (encoder.input(), encoder.latest_texture()) {
        (Some(input), Some(latest)) => {
            device.copy(input, latest);
            encoder.submit(index, false)
        }
        _ => encoder.repeat_once_free(index, output),
    }
}

/// Where the encoder's frames go, as `config` asks: into RAM; into a file on disk, which `live`
/// is given as its store; into GPU memory written by the encoder itself, when the session is
/// on a device's primary CUDA context; or, with any other session, into GPU memory through a
/// Direct3D device of the buffer's own, which `live` is given as its store. When the disk or
/// GPU memory cannot be had the buffer stays in RAM, with a warning.
fn choose_output(config: &Config, on: &EncoderDevice, live: &Live) -> Output {
    match config.buffer_memory {
        Memory::Ram => return Output::Memory,
        Memory::Disk => {
            keep_on_disk(config, live);
            return Output::Memory;
        }
        Memory::Vram => {}
    }
    let bytes = config.estimated_buffer_bytes();
    if let EncoderDevice::Cuda {
        primary: Some(device),
        ..
    } = *on
    {
        match CudaRing::create(device, bytes) {
            Ok(ring) => {
                log::info(ring.description());
                return Output::VideoMemory(ring);
            }
            Err(error) => log::warning(format!(
                "The replay buffer could not be placed in CUDA memory ({error}); trying a Direct3D buffer"
            )),
        }
    }
    match vram::VideoMemory::create(bytes) {
        Ok(store) => {
            log::info(store.description());
            live.set_store(Box::new(store));
        }
        Err(error) => log::warning(format!(
            "The replay buffer could not be placed in GPU memory ({error}); keeping it in RAM"
        )),
    }
    Output::Memory
}

/// Gives `live` a store in the buffer file in the state directory, sized for the buffer
/// `config` asks for; when the drive lacks the room or the file cannot be made, the buffer
/// stays in RAM, with a warning.
fn keep_on_disk(config: &Config, live: &Live) {
    match DiskStore::create(&storage::buffer_file(), config.estimated_buffer_bytes()) {
        Ok(store) => {
            log::info(store.description());
            live.set_store(Box::new(store));
        }
        Err(error) => log::warning(format!(
            "The replay buffer could not be placed on disk ({error}); keeping it in RAM"
        )),
    }
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

    /// Whether `encoded` is a keyframe, which a clip can start with. A frame left in video
    /// memory carries no picture type, so its first bytes are parsed for every frame.
    fn classify(&mut self, encoded: &Encoded) -> bool {
        let idr = encoded.is_idr();
        self.driver_flags_keyframes |= idr;
        idr || (!self.driver_flags_keyframes && replay::is_keyframe(self.codec, encoded.head()))
    }
}

/// What a tick left in the encoder's input.
enum Input {
    /// A new picture to submit.
    Ready,
    /// The screen did not change: the newest picture is to be submitted again.
    Repeat,
    /// A new picture to submit from the capture's surface with this index.
    InSurface(usize),
    /// Nothing has been captured yet; there is no picture to repeat.
    Empty,
    /// The capture ended the session.
    Ended(Ended),
}

/// Fills the input of the slot [`Encoder::reserve`] freed with the newest captured picture of
/// the encoded size, unless the capture left it in a surface of its own or the screen did not
/// change.
fn fill_input(
    device: &Device,
    capture: &mut dyn capture::Source,
    encoder: &Encoder,
) -> Result<Input, CaptureError> {
    let Settings { width, height, .. } = encoder.settings();
    let frame = match capture.take(device, encoder.input(), width, height) {
        Ok(frame) => frame,
        Err(CaptureError::Closed) => return Ok(Input::Ended(Ended::Closed)),
        Err(error) => return Err(error),
    };
    Ok(match frame {
        Frame::Fresh => Input::Ready,
        Frame::InSurface(surface) => Input::InSurface(surface),
        Frame::Unchanged if encoder.has_latest() => Input::Repeat,
        Frame::Unchanged => Input::Empty,
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
