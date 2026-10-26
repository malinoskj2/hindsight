//! The rolling buffers of encoded video and captured sound a recording session shares with
//! the rest of the application, and saving them to an MP4 file.

use crate::{audio::EncodeError, config::Codec, log};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};
use thiserror::Error;

mod annexb;
mod audio;
mod av1;
mod clip_name;
mod disk;
mod h264;
mod hevc;
mod mp4;
mod ring;
mod store;

pub(crate) use audio::{
    AudioClip, AudioFormat, CLOCK_RATE, FRAME_SAMPLES, PcmClip, Source, samples_of, ticks_of,
};
use audio::{AudioRings, tick_time};
pub(crate) use clip_name::ClipName;
#[cfg(windows)]
pub(crate) use disk::DiskStore;
use mp4::{DecoderConfig, MuxError, Visit};
use ring::Ring;
use store::Entry;
#[cfg(windows)]
pub(crate) use store::{
    BufferTooLarge, CHUNK_BYTES, PAGE_BYTES, Page, Pages, Slot, pages_within, pieces,
};
pub(crate) use store::{Store, StoreError};

const PARTIAL_EXTENSION: &str = "partial";
/// The number of clips saved within one second before a save gives up looking for a free name.
const NAME_ATTEMPTS: u32 = 1000;
const WRITE_BUFFER_BYTES: usize = 1 << 20;

/// One encoded frame: an H.264 or HEVC access unit in Annex B form, or an AV1 temporal unit.
#[derive(Debug)]
pub(crate) struct Packet {
    /// The tick of the session's frame clock. Indices rise, with gaps for skipped ticks.
    pub(crate) index: u64,
    pub(crate) keyframe: bool,
    pub(crate) data: Box<[u8]>,
}

/// Whether a frame brought new screen content or repeated the last picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Picture {
    Fresh,
    Repeated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stream {
    pub(crate) codec: Codec,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_rate: u32,
    pub(crate) matrix: Matrix,
}

/// The matrix coefficients the YUV was derived with. Primaries and transfer are always BT.709,
/// limited range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Matrix {
    /// The matrix NVENC converts BGRA input with.
    #[default]
    Bt709,
}

impl Matrix {
    /// Returns the H.273 code point, as used by the VUI, the AV1 sequence header and the `colr`
    /// atom.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Bt709 => 1,
        }
    }
}

/// The frames and bytes the buffer holds, and whether they are in the store outside RAM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) frames: u64,
    pub(crate) bytes: u64,
    pub(crate) stored: bool,
}

pub(crate) struct Live {
    ring: Mutex<Ring>,
    stream: OnceLock<Stream>,
    counters: Counters,
    on_stream: OnceLock<Box<dyn Fn() + Send + Sync>>,
    origin: OnceLock<i64>,
    audio: OnceLock<Arc<AudioFormat>>,
    sound: Mutex<AudioRings>,
    /// The logical processor of the capture thread, or `u32::MAX` until it has reported one.
    #[cfg_attr(
        all(not(windows), test),
        expect(
            dead_code,
            reason = "only the Windows capture and audio threads use it"
        )
    )]
    capture_processor: AtomicU32,
}

/// Counts of the frames pushed, read without taking the ring's lock.
struct Counters {
    encoded: AtomicU64,
    /// Frames that brought new screen content.
    fresh: AtomicU64,
    bytes: AtomicU64,
    /// The index of the oldest buffered packet, or `u64::MAX`. The sound is trimmed to it.
    oldest: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            encoded: AtomicU64::new(0),
            fresh: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            oldest: AtomicU64::new(u64::MAX),
        }
    }

    fn frame(&self, bytes: usize, oldest: Option<u64>, picture: Picture) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.oldest
            .store(oldest.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.encoded.fetch_add(1, Ordering::Relaxed);
        if picture == Picture::Fresh {
            self.fresh.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Live {
    pub(crate) fn new(span: u64) -> Self {
        Self {
            ring: Mutex::new(Ring::new(span)),
            stream: OnceLock::new(),
            counters: Counters::new(),
            on_stream: OnceLock::new(),
            origin: OnceLock::new(),
            audio: OnceLock::new(),
            sound: Mutex::new(AudioRings::new()),
            capture_processor: AtomicU32::new(u32::MAX),
        }
    }

    /// Records the logical processor the capture thread runs on, so the audio thread can avoid
    /// it.
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture thread uses it")
    )]
    pub(crate) fn note_capture_processor(&self, processor: u32) {
        self.capture_processor.store(processor, Ordering::Relaxed);
    }

    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows audio thread uses it")
    )]
    pub(crate) fn capture_processor(&self) -> Option<u32> {
        let processor = self.capture_processor.load(Ordering::Relaxed);
        (processor != u32::MAX).then_some(processor)
    }

    /// Runs `callback` on the capture thread once the stream is recorded.
    pub(crate) fn on_stream(&self, callback: impl Fn() + Send + Sync + 'static) {
        set_once(&self.on_stream, Box::new(callback), "stream callback");
    }

    pub(crate) fn set_stream(&self, stream: Stream) {
        let stream = Stream {
            frame_rate: stream.frame_rate.max(1),
            ..stream
        };
        if set_once(&self.stream, stream, "stream")
            && let Some(callback) = self.on_stream.get()
        {
            callback();
        }
    }

    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream.get().copied()
    }

    /// Sets the QPC time, in 100 ns units, of the frame clock's first tick.
    pub(crate) fn set_origin(&self, time: i64) {
        set_once(&self.origin, time, "frame clock's origin");
    }

    pub(crate) fn set_store(&self, store: Box<dyn Store>) {
        self.ring().set_store(store);
    }

    pub(crate) fn push(&self, packet: Packet, picture: Picture) {
        self.push_entry(packet.into(), picture);
    }

    #[cfg(windows)]
    pub(crate) fn push_stored(&self, index: u64, keyframe: bool, slot: Slot, picture: Picture) {
        self.push_entry(
            Entry {
                index,
                keyframe,
                data: store::Data::Stored(slot),
            },
            picture,
        );
    }

    fn push_entry(&self, entry: Entry, picture: Picture) {
        let bytes = entry.length();
        let (oldest, refused) = {
            let mut ring = self.ring();
            let refused = ring.push(entry).err();
            (ring.first_index(), refused)
        };
        if let Some(error) = refused {
            log::warning(format!(
                "Buffer · lost its place outside RAM ({}) · continues in RAM",
                log::chain(&error)
            ));
        }
        self.counters.frame(bytes, oldest, picture);
    }

    pub(crate) fn set_audio(&self, format: AudioFormat) {
        set_once(&self.audio, Arc::new(format), "audio format");
    }

    pub(crate) fn push_audio(&self, source: Source, start: u64, data: Arc<[i16]>) {
        let Some(format) = self.audio.get() else {
            return;
        };
        let video_start = self.video_time(self.counters.oldest.load(Ordering::Relaxed));
        let mut sound = self.sound();
        let ring = sound.ring(source);
        ring.push(format, start, data);
        ring.trim(format, video_start);
    }

    fn video_time(&self, index: u64) -> Option<i64> {
        let origin = *self.origin.get()?;
        let frame_rate = self.stream()?.frame_rate;
        (index != u64::MAX).then(|| tick_time(origin, frame_rate, index))
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.counters.bytes.load(Ordering::Relaxed)
    }

    /// Returns the number of frames encoded, and how many of them were fresh.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (
            self.counters.encoded.load(Ordering::Relaxed),
            self.counters.fresh.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn held(&self) -> Held {
        let ring = self.ring();
        Held {
            frames: ring.frames(),
            bytes: ring.bytes() as u64,
            stored: ring.stored(),
        }
    }

    /// Cheap to call: stored packets are read back, and the sound encoded, only when a save
    /// writes them.
    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
        let stream = self.stream()?;
        let entries = self.ring().snapshot();
        let (first, last) = (entries.first()?.index, entries.last()?.index);
        let audio = self.audio_between(first, last + 1);
        Some(Snapshot {
            stream,
            entries,
            audio,
        })
    }

    #[cfg(test)]
    pub(crate) fn clip(&self) -> Option<Clip> {
        self.snapshot().map(|snapshot| snapshot.clip().unwrap())
    }

    fn audio_between(&self, first: u64, end: u64) -> Option<PcmClip> {
        let format = self.audio.get()?;
        let (start, end) = (self.video_time(first)?, self.video_time(end)?);
        Some(audio::cut(&self.sound(), format, start, end))
    }

    fn ring(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sound(&self) -> MutexGuard<'_, AudioRings> {
        self.sound.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Sets `lock`, which a session sets once. A second set is a caller's bug and keeps the first
/// value.
fn set_once<T>(lock: &OnceLock<T>, value: T, what: &str) -> bool {
    let set = lock.set(value).is_ok();
    debug_assert!(set, "the {what} was set twice");
    set
}

pub(crate) struct Snapshot {
    stream: Stream,
    entries: Vec<Arc<Entry>>,
    audio: Option<PcmClip>,
}

impl Snapshot {
    pub(crate) fn seconds(&self) -> f64 {
        seconds(
            self.entries.first().map(|entry| entry.index),
            self.entries.last().map(|entry| entry.index),
            self.stream.frame_rate,
        )
    }

    #[cfg(test)]
    pub(crate) fn clip(self) -> Result<Clip, StoreError> {
        Ok(Clip {
            stream: self.stream,
            packets: store::fetch(&self.entries)?,
            audio: self.audio,
        })
    }
}

/// Stored packets are read back a run at a time on each pass, never all at once.
impl mp4::Packets for [Arc<Entry>] {
    fn visit(&self, each: &mut Visit<'_>) -> Result<(), MuxError> {
        store::each(self, &mut |entry, data| {
            each(entry.index, entry.keyframe, data)
        })
    }
}

#[cfg(test)]
pub(crate) struct Clip {
    pub(crate) stream: Stream,
    pub(crate) packets: Vec<Arc<Packet>>,
    pub(crate) audio: Option<PcmClip>,
}

#[cfg(test)]
impl Clip {
    pub(crate) fn seconds(&self) -> f64 {
        seconds(
            self.packets.first().map(|packet| packet.index),
            self.packets.last().map(|packet| packet.index),
            self.stream.frame_rate,
        )
    }
}

fn seconds(first: Option<u64>, last: Option<u64>, frame_rate: u32) -> f64 {
    match (first, last) {
        (Some(first), Some(last)) => (last - first + 1) as f64 / f64::from(frame_rate),
        _ => 0.0,
    }
}

/// The reason a save failed. The message names only the failed step, and the cause is its
/// `source()`, so it is displayed with [`log::chain`].
#[derive(Debug, Error)]
pub(crate) enum SaveError {
    #[error("{step} {}", path.display())]
    Io {
        step: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Mux(#[from] MuxError),
}

impl SaveError {
    fn io(step: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io {
            step,
            path: path.to_owned(),
            source,
        }
    }
}

pub(crate) fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::H264 => h264::is_keyframe(data),
        Codec::Hevc => hevc::is_keyframe(data),
        Codec::Av1 => av1::is_keyframe(data),
    }
}

/// Returns the `avcC`, `hvcC` or `av1C` record from the sequence header in `data`.
fn configuration(codec: Codec, data: &[u8]) -> Option<DecoderConfig> {
    match codec {
        Codec::H264 => h264::configuration(data).map(DecoderConfig::Avc),
        Codec::Hevc => hevc::configuration(data).map(DecoderConfig::Hevc),
        Codec::Av1 => av1::configuration(data).map(DecoderConfig::Av1),
    }
}

fn sample_parts(codec: Codec, data: &[u8]) -> Vec<&[u8]> {
    match codec {
        Codec::H264 => h264::sample_units(data),
        Codec::Hevc => hevc::sample_units(data),
        Codec::Av1 => av1::sample_obus(data),
    }
}

#[derive(Debug)]
pub(crate) struct Saved {
    pub(crate) path: PathBuf,
    /// The reason the buffered sound is not in the clip, or `None` when it is, or when none was
    /// buffered.
    pub(crate) sound_error: Option<EncodeError>,
}

/// Writes `snapshot` to `directory` as `hindsight_<date>_<time>.mp4`. The file is written under
/// a `.partial` name and renamed to its final one only once complete. Sound that cannot be
/// encoded is left out rather than failing the save.
pub(crate) fn save(snapshot: &Snapshot, directory: &Path) -> Result<Saved, SaveError> {
    fs::create_dir_all(directory).map_err(SaveError::io("create the clip folder", directory))?;
    let name = ClipName::now();
    let partial = directory
        .join(name.file_name())
        .with_extension(PARTIAL_EXTENSION);
    let result = write_file(snapshot, &partial).and_then(|sound_error| {
        claim_name(directory, name, &partial).map(|path| Saved { path, sound_error })
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Moves the finished `partial` to the first attempt of `name` that no file has. Each name is
/// claimed by creating it, so a file that appears in the meantime is never replaced.
fn claim_name(directory: &Path, name: ClipName, partial: &Path) -> Result<PathBuf, SaveError> {
    let mut name = name;
    for _ in 0..NAME_ATTEMPTS {
        let path = directory.join(name.file_name());
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(claimed) => {
                drop(claimed);
                return match fs::rename(partial, &path) {
                    Ok(()) => Ok(path),
                    Err(error) => {
                        let _ = fs::remove_file(&path);
                        Err(SaveError::io("finish", &path)(error))
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => name = name.next(),
            Err(error) => return Err(SaveError::io("name", &path)(error)),
        }
    }
    Err(SaveError::io("name", &directory.join(name.file_name()))(
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "every name for this second is taken",
        ),
    ))
}

/// Writes the clip to `path` and returns the reason the buffered sound was left out, if it was.
fn write_file(snapshot: &Snapshot, path: &Path) -> Result<Option<EncodeError>, SaveError> {
    let (audio, sound_error) = match snapshot.audio.as_ref().map(crate::audio::encode) {
        Some(Ok(audio)) => (Some(audio), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    let file = File::create(path).map_err(SaveError::io("create", path))?;
    let mut output = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    mp4::write_from(
        &mut output,
        &snapshot.stream,
        &snapshot.entries[..],
        audio.as_ref(),
    )?;
    sync(output).map_err(SaveError::io("finish", path))?;
    Ok(sound_error)
}

fn sync(output: BufWriter<File>) -> io::Result<()> {
    let file = output
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

#[cfg(test)]
mod tests;
