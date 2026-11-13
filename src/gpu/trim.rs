//! Trimming on the GPU: a clip's pictures decoded small for the trim view, and a cut of it
//! decoded and encoded afresh with NVENC, its sound copied.

use crate::{
    config::{Codec, Container},
    gpu::{
        capture::{EncoderDevice, Format, Surface},
        cuda::{ContextHold, CudaError, Driver},
        device::{Device, DeviceError},
        nvdec::{DecodeError, Decoder, Output, Picture, Pixels, Size},
        nvenc::{Encoded, Encoder, NvencError, Payload, Settings},
        pacer::FrameIndex,
    },
    log,
    replay::{self, DemuxError, Movie, Packet, SaveError, Stream, read_place},
    share::{self, ShareError},
    trim::{Cut, Output as Encode, Rate, Timeline, nv12_to_bgra},
};
use std::{
    collections::VecDeque,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use thiserror::Error;

/// Frames NVENC may read at once, each holding a decoded picture.
const IN_FLIGHT: usize = 3;
pub(crate) const PROGRESS_SCALE: u32 = 1000;

/// The reason a trim failed. The message names only the failed step, and the cause is its
/// `source()`.
#[derive(Debug, Error)]
#[error(transparent)]
pub(crate) struct TrimError(#[from] TrimFailure);

impl TrimError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self.0, TrimFailure::Cancelled)
    }
}

#[derive(Debug, Error)]
enum TrimFailure {
    #[error("read the clip")]
    Demux(#[source] DemuxError),
    #[error("read the clip")]
    Read(#[source] io::Error),
    #[error("open the GPU")]
    Device(#[source] DeviceError),
    #[error("{0}")]
    Cuda(&'static str, #[source] CudaError),
    #[error("decode the clip")]
    Decode(#[source] DecodeError),
    #[error("encode the cut")]
    Encode(#[source] NvencError),
    #[error("the cut holds no frames")]
    Empty,
    #[error("the trim was cancelled")]
    Cancelled,
    #[error("the size is too small for this cut's length and sound")]
    SizeTooSmall,
    #[error("NVENC could not fit the cut within the size")]
    TooLarge,
    #[error("keep the cut within the size")]
    Size(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("save the cut")]
    Save(#[source] SaveError),
}

impl From<DemuxError> for TrimError {
    fn from(error: DemuxError) -> Self {
        Self(TrimFailure::Demux(error))
    }
}

impl From<io::Error> for TrimError {
    fn from(error: io::Error) -> Self {
        Self(TrimFailure::Read(error))
    }
}

impl From<DecodeError> for TrimError {
    fn from(error: DecodeError) -> Self {
        Self(TrimFailure::Decode(error))
    }
}

impl From<NvencError> for TrimError {
    fn from(error: NvencError) -> Self {
        Self(TrimFailure::Encode(error))
    }
}

fn cuda_failed(step: &'static str) -> impl Fn(CudaError) -> TrimError {
    move |error| TrimError(TrimFailure::Cuda(step, error))
}

/// The primary context of the CUDA device that is `device`'s adapter, or of device 0 when none
/// matches.
fn cuda_context(device: &Device) -> Result<Arc<ContextHold>, TrimError> {
    let cuda = Driver::load().map_err(cuda_failed("load the CUDA driver"))?;
    let matched = device
        .luid()
        .ok()
        .and_then(|luid| cuda.device_with_luid(luid).ok().flatten());
    let ordinal = match matched {
        Some(ordinal) => ordinal,
        None => cuda
            .first_device()
            .map_err(cuda_failed("open CUDA device 0"))?,
    };
    let hold = cuda
        .retain_primary_context(ordinal)
        .map_err(cuda_failed("open the CUDA context"))?;
    Ok(Arc::new(hold))
}

/// A picture for the trim view, as top-down BGRA.
pub(crate) struct Frame {
    /// Clock ticks from the clip's first picture.
    pub(crate) time: i64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bgra: Vec<u8>,
}

/// A clip decoded in order, small enough to show, from wherever it was last sought.
pub(crate) struct Preview {
    decoder: Decoder,
    movie: Movie,
    file: File,
    /// The next sample to decode, in decode order.
    next: usize,
    flushed: bool,
    /// A picture decoded past the time sought, shown next.
    held: Option<Picture>,
}

impl Preview {
    /// Opens the clip with its pictures scaled to fit within `bounds`.
    pub(crate) fn open(path: &Path, bounds: (u32, u32)) -> Result<Self, TrimError> {
        let (movie, file) = Movie::open(path)?;
        let device = Device::create().map_err(|error| TrimError(TrimFailure::Device(error)))?;
        let hold = cuda_context(&device)?;
        let decoder = Decoder::open(hold, movie.video.stream.codec, Some(bounds), Output::Host)?;
        Ok(Self {
            decoder,
            movie,
            file,
            next: 0,
            flushed: false,
            held: None,
        })
    }

    pub(crate) fn movie(&self) -> &Movie {
        &self.movie
    }

    /// Returns the picture shown at `time`, after which [`Self::next_frame`] goes on from there.
    pub(crate) fn seek(&mut self, time: i64) -> Result<Option<Frame>, TrimError> {
        self.decoder.restart()?;
        self.next = self.movie.video.keyframe_before(time);
        self.flushed = false;
        self.held = None;
        let mut shown = None;
        while let Some(picture) = self.next_picture()? {
            if picture.time <= time {
                shown = Some(picture);
                continue;
            }
            // Before the first picture, the first is shown.
            if shown.is_none() {
                shown = Some(picture);
            } else {
                self.held = Some(picture);
            }
            break;
        }
        Ok(shown.and_then(|picture| self.frame(picture)))
    }

    /// Returns the next picture in display order, or `None` after the last.
    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>, TrimError> {
        Ok(self.next_picture()?.and_then(|picture| self.frame(picture)))
    }

    fn next_picture(&mut self) -> Result<Option<Picture>, TrimError> {
        if let Some(held) = self.held.take() {
            return Ok(Some(held));
        }
        loop {
            if let Some(picture) = self.decoder.next() {
                return Ok(Some(picture));
            }
            let video = &self.movie.video;
            if let Some(sample) = video.samples.get(self.next).copied() {
                self.next += 1;
                let data = read_place(&mut self.file, sample.place)?;
                self.decoder
                    .decode(&video.decoder_input(&data, sample.keyframe), sample.time)?;
            } else if !self.flushed {
                self.flushed = true;
                self.decoder.finish()?;
            } else {
                return Ok(None);
            }
        }
    }

    fn frame(&self, picture: Picture) -> Option<Frame> {
        let Pixels::Host(nv12) = picture.pixels else {
            return None;
        };
        let size = self.decoder.size()?;
        let (width, height) = (size.width as usize, size.height as usize);
        let mut bgra = vec![0; width * height * 4];
        nv12_to_bgra(
            &nv12,
            size.pitch as usize,
            (width, height),
            self.movie.video.stream.matrix,
            &mut bgra,
        );
        Some(Frame {
            time: picture.time,
            width: size.width,
            height: size.height,
            bgra,
        })
    }
}

/// The encoding policy for an export.
pub(crate) enum Quality {
    Trim(Encode),
    /// H.264 in MP4 at this many bits per second, at most 1080p and 60 fps, with the main sound
    /// alone.
    Share(u32),
}

/// A cut to encode and save beside the clips.
pub(crate) struct Job {
    pub(crate) source: PathBuf,
    pub(crate) cut: Cut,
    pub(crate) directory: PathBuf,
    pub(crate) quality: Quality,
}

/// One encoding of a cut.
#[derive(Clone, Copy)]
struct Plan {
    codec: Codec,
    container: Container,
    bitrate: u32,
    bounds: Option<(u32, u32)>,
    max_frame_rate: Option<u32>,
    main_sound_only: bool,
}

impl Plan {
    const fn trim(output: Encode, bitrate: u32) -> Self {
        Self {
            codec: output.codec,
            container: output.container,
            bitrate,
            bounds: None,
            max_frame_rate: None,
            main_sound_only: false,
        }
    }

    const fn share(bitrate: u32) -> Self {
        Self {
            codec: Codec::H264,
            container: Container::Mp4,
            bitrate,
            bounds: Some((1920, 1080)),
            max_frame_rate: Some(60),
            main_sound_only: true,
        }
    }
}

/// Encodes the cut and saves it, reporting its progress in thousandths. Returns the saved file.
/// A cut held to a size is encoded again at a lower bitrate while it comes out larger.
pub(crate) fn export(
    job: &Job,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<PathBuf, TrimError> {
    let output = match job.quality {
        Quality::Share(bitrate) => return encode(job, Plan::share(bitrate), progress, cancel),
        Quality::Trim(output) => output,
    };
    let megabytes = match output.rate {
        Rate::Bitrate(mbps) => {
            let bitrate = mbps.saturating_mul(1_000_000);
            return encode(job, Plan::trim(output, bitrate), progress, cancel);
        }
        Rate::Size(megabytes) => megabytes,
    };
    let (movie, _) = Movie::open(&job.source)?;
    share::encode_with_limit(
        share::limit_bytes(megabytes),
        job.cut.sound_bytes(&movie.sounds),
        job.cut.duration(),
        cancel,
        |bitrate| Ok(encode(job, Plan::trim(output, bitrate), progress, cancel)?),
    )
    .map_err(|error| {
        let error = match error.downcast::<TrimError>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        TrimError(match error.downcast_ref::<ShareError>() {
            Some(ShareError::Budget) => TrimFailure::SizeTooSmall,
            Some(ShareError::TooLarge) => TrimFailure::TooLarge,
            Some(ShareError::Cancelled) => TrimFailure::Cancelled,
            None => TrimFailure::Size(error.into()),
        })
    })
}

fn encode(
    job: &Job,
    plan: Plan,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<PathBuf, TrimError> {
    let (movie, mut file) = Movie::open(&job.source)?;
    let video = &movie.video;
    let range = job.cut.decoded(video);
    if range.is_empty() {
        return Err(TrimError(TrimFailure::Empty));
    }
    let device = Device::create().map_err(|error| TrimError(TrimFailure::Device(error)))?;
    let hold = cuda_context(&device)?;
    let _current = hold
        .make_current()
        .map_err(cuda_failed("make the CUDA context current"))?;
    let source = Stream {
        codec: plan.codec,
        frame_rate: plan.max_frame_rate.map_or(video.stream.frame_rate, |most| {
            video.stream.frame_rate.min(most)
        }),
        ..video.stream
    };
    let (bounds, bitrate) = (plan.bounds, plan.bitrate);
    let container = plan.container;
    let sounds = if plan.main_sound_only {
        &movie.sounds[..movie.sounds.len().min(1)]
    } else {
        &movie.sounds[..]
    };
    progress.store(0, Ordering::Relaxed);
    let mut decoder = Decoder::open(
        Arc::clone(&hold),
        video.stream.codec,
        bounds,
        Output::Device,
    )?;
    let mut encoding: Option<Encoding> = None;
    let total = range.len();
    for (done, number) in range.enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(TrimError(TrimFailure::Cancelled));
        }
        let sample = video.samples[number];
        let data = read_place(&mut file, sample.place)?;
        decoder.decode(&video.decoder_input(&data, sample.keyframe), sample.time)?;
        encode_decoded(
            &mut decoder,
            &mut encoding,
            job.cut,
            &device,
            &hold,
            source,
            bitrate,
        )?;
        progress.store(permille(done + 1, total), Ordering::Relaxed);
    }
    decoder.finish()?;
    encode_decoded(
        &mut decoder,
        &mut encoding,
        job.cut,
        &device,
        &hold,
        source,
        bitrate,
    )?;
    let (stream, packets) = encoding
        .ok_or(TrimError(TrimFailure::Empty))?
        .finish(&mut decoder)?;
    let mut audio = Vec::new();
    for sound in sounds {
        if let Some(clip) = job.cut.read_sound(&mut file, sound)? {
            audio.push(clip);
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(TrimError(TrimFailure::Cancelled));
    }
    replay::save_cut(&job.directory, container, &stream, &packets, &audio)
        .map_err(|error| TrimError(TrimFailure::Save(error)))
}

fn permille(done: usize, total: usize) -> u32 {
    let scaled = done as u64 * u64::from(PROGRESS_SCALE) / total.max(1) as u64;
    u32::try_from(scaled).unwrap_or(PROGRESS_SCALE)
}

/// Encodes the decoded pictures the cut shows and hands back the rest.
fn encode_decoded(
    decoder: &mut Decoder,
    encoding: &mut Option<Encoding>,
    cut: Cut,
    device: &Device,
    hold: &Arc<ContextHold>,
    source: Stream,
    bitrate: u32,
) -> Result<(), TrimError> {
    while let Some(picture) = decoder.next() {
        let Pixels::Device(buffer) = picture.pixels else {
            continue;
        };
        if !cut.shows(picture.time) {
            decoder.recycle(buffer);
            continue;
        }
        let size = decoder.size().ok_or(TrimError(TrimFailure::Empty))?;
        let encoding = match encoding {
            Some(encoding) => encoding,
            None => encoding.insert(Encoding::open(device, hold, source, size, bitrate)?),
        };
        encoding.add(
            decoder,
            buffer,
            cut.frame_index(picture.time, source.frame_rate),
        )?;
    }
    Ok(())
}

/// NVENC reading the decoder's buffers in place.
struct Encoding {
    encoder: Encoder,
    stream: Stream,
    pitch: u32,
    /// The decoder buffers NVENC may still read, oldest first.
    in_flight: VecDeque<usize>,
    last_index: Option<u64>,
    packets: Vec<Arc<Packet>>,
}

impl Encoding {
    /// Encodes in the clip's codec, or in H.264 when this GPU's NVENC has no encoder for it.
    fn open(
        device: &Device,
        hold: &Arc<ContextHold>,
        source: Stream,
        size: Size,
        bitrate: u32,
    ) -> Result<Self, TrimError> {
        let stream = Stream {
            width: size.width,
            height: size.height,
            ..source
        };
        let open = |codec: Codec| {
            Encoder::open(
                &device.d3d,
                EncoderDevice::Cuda {
                    hold: Arc::clone(hold),
                    ring: None,
                },
                Settings {
                    codec,
                    width: stream.width,
                    height: stream.height,
                    frame_rate: stream.frame_rate,
                    bitrate_bps: bitrate,
                    matrix: stream.matrix,
                },
            )
        };
        let (encoder, codec) = match open(stream.codec) {
            Err(NvencError::Unsupported(codec)) if codec != Codec::H264 => {
                log::info(format!(
                    "Trim · this GPU encodes no {} · the cut is H.264",
                    codec.name()
                ));
                (open(Codec::H264)?, Codec::H264)
            }
            opened => (opened?, stream.codec),
        };
        Ok(Self {
            encoder,
            stream: Stream { codec, ..stream },
            pitch: size.pitch,
            in_flight: VecDeque::with_capacity(IN_FLIGHT + 1),
            last_index: None,
            packets: Vec::new(),
        })
    }

    /// Encodes the picture in the decoder's `buffer` as frame `index`. A picture that falls on
    /// the tick of the one before is dropped.
    fn add(&mut self, decoder: &mut Decoder, buffer: usize, index: u64) -> Result<(), TrimError> {
        if self.last_index.is_some_and(|last| index <= last) {
            decoder.recycle(buffer);
            return Ok(());
        }
        if !self.encoder.has_surface(buffer) {
            let pixels = decoder
                .buffer(buffer)
                .ok_or(TrimError(TrimFailure::Empty))?;
            self.encoder.register_surface(
                buffer,
                &Surface::CudaBuffer {
                    buffer: Arc::clone(pixels),
                    pitch: self.pitch,
                    format: Format::Nv12,
                },
            )?;
        }
        let codec = self.stream.codec;
        let packets = &mut self.packets;
        let mut output = |encoded: Encoded| collect(codec, packets, encoded);
        self.encoder.reserve(&mut output)?;
        self.encoder
            .submit_surface(FrameIndex::new(index), buffer)?;
        self.last_index = Some(index);
        self.in_flight.push_back(buffer);
        while self.in_flight.len() > IN_FLIGHT {
            if let Some(oldest) = self.in_flight.pop_front() {
                self.encoder.release_surface(oldest, &mut output)?;
                decoder.recycle(oldest);
            }
        }
        self.encoder.collect(&mut output)?;
        Ok(())
    }

    fn finish(mut self, decoder: &mut Decoder) -> Result<(Stream, Vec<Arc<Packet>>), TrimError> {
        let codec = self.stream.codec;
        let packets = &mut self.packets;
        let mut output = |encoded: Encoded| collect(codec, packets, encoded);
        while let Some(buffer) = self.in_flight.pop_front() {
            self.encoder.release_surface(buffer, &mut output)?;
            decoder.recycle(buffer);
        }
        self.encoder.finish(&mut output)?;
        Ok((self.stream, std::mem::take(&mut self.packets)))
    }
}

fn collect(codec: Codec, packets: &mut Vec<Arc<Packet>>, encoded: Encoded) {
    let keyframe = encoded.is_idr() || replay::is_keyframe(codec, encoded.head());
    let index = encoded.index.get();
    if let Payload::Memory(data) = encoded.data {
        packets.push(Arc::new(Packet {
            index,
            keyframe,
            data,
        }));
    }
}

/// Makes a complete, size-checked MP4 for pasting. NVDEC scales on the GPU and NVENC writes
/// H.264; the first AAC track carries the mix, without the separate editing-only microphone.
pub(crate) fn share(path: &Path, limit: u64, cancel: &AtomicBool) -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    let (movie, _) = Movie::open(path).context("read the saved clip")?;
    let audio_bytes = movie.sounds.first().map_or(0, |sound| {
        sound.frames.iter().map(|frame| u64::from(frame.size)).sum()
    });
    let cut = Timeline::of(&movie.video).whole();
    let directory = path
        .parent()
        .context("find the clip's folder")?
        .join("shareable");
    share::encode_with_limit(
        limit,
        audio_bytes,
        movie.video.duration,
        cancel,
        |bitrate| {
            let job = Job {
                source: path.to_owned(),
                cut,
                directory: directory.clone(),
                quality: Quality::Share(bitrate),
            };
            Ok(export(&job, &AtomicU32::new(0), cancel)?)
        },
    )
}
