//! The rolling buffers of encoded video and captured sound a recording session shares with
//! the rest of the application, and saving them to an MP4 file.

use crate::{config::Codec, log};
use std::{
    fs::{self, File},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

mod annexb;
mod audio;
mod av1;
mod disk;
mod h264;
mod hevc;
mod mp4;
mod ring;
mod store;

pub(crate) use audio::{
    AudioClip, AudioFormat, CLOCK_RATE, FRAME_SAMPLES, PcmClip, samples_of, ticks_of,
};
use audio::{AudioRing, tick_time};
#[cfg(windows)]
pub(crate) use disk::DiskStore;
use mp4::{MuxError, Visit};
use ring::Ring;
use store::Entry;
#[cfg(windows)]
pub(crate) use store::{CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, Page, Pages, Slot, reserved_pages};
pub(crate) use store::{Store, StoreError};

const CLIP_PREFIX: &str = "hindsight";
const CLIP_EXTENSION: &str = "mp4";
const PARTIAL_EXTENSION: &str = "partial";
const WRITE_BUFFER_BYTES: usize = 1 << 20;

/// One encoded frame: an H.264 or HEVC access unit in Annex B form, or an AV1 temporal unit.
#[derive(Debug)]
pub(crate) struct Packet {
    /// Tick of the session's frame clock; rising, with gaps for skipped ticks.
    pub(crate) index: u64,
    pub(crate) keyframe: bool,
    pub(crate) data: Box<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stream {
    pub(crate) codec: Codec,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_rate: u32,
    pub(crate) matrix: Matrix,
}

/// Matrix coefficients the YUV was derived with; primaries and transfer are always BT.709,
/// range limited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Matrix {
    /// What NVENC converts BGRA input with.
    #[default]
    Bt709,
}

impl Matrix {
    /// The H.273 code point, as the VUI, the AV1 sequence header and the `colr` atom use.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Bt709 => 1,
        }
    }
}

pub(crate) struct Live {
    ring: Mutex<Ring>,
    stream: OnceLock<Stream>,
    counters: Counters,
    on_stream: OnceLock<Box<dyn Fn() + Send + Sync>>,
    origin: OnceLock<i64>,
    audio: OnceLock<Arc<AudioFormat>>,
    sound: Mutex<AudioRing>,
}

/// Tallies of the frames pushed, read without the ring's lock.
struct Counters {
    encoded: AtomicU64,
    /// Frames that brought new screen content.
    fresh: AtomicU64,
    bytes: AtomicU64,
    /// Index of the oldest buffered packet, or `u64::MAX`; the sound is trimmed to it.
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

    fn frame(&self, bytes: usize, oldest: Option<u64>, fresh: bool) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.oldest
            .store(oldest.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.encoded.fetch_add(1, Ordering::Relaxed);
        if fresh {
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
            sound: Mutex::new(AudioRing::default()),
        }
    }

    /// Runs `callback` on the capture thread once the stream is recorded.
    pub(crate) fn on_stream(&self, callback: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_stream.set(Box::new(callback));
    }

    pub(crate) fn set_stream(&self, stream: Stream) {
        let stream = Stream {
            frame_rate: stream.frame_rate.max(1),
            ..stream
        };
        if self.stream.set(stream).is_ok()
            && let Some(callback) = self.on_stream.get()
        {
            callback();
        }
    }

    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream.get().copied()
    }

    /// QPC time, in 100 ns units, of the frame clock's first tick.
    pub(crate) fn set_origin(&self, time: i64) {
        let _ = self.origin.set(time);
    }

    pub(crate) fn set_store(&self, store: Box<dyn Store>) {
        self.ring().set_store(store);
    }

    pub(crate) fn push(&self, packet: Packet, fresh: bool) {
        self.push_entry(packet.into(), fresh);
    }

    #[cfg(windows)]
    pub(crate) fn push_stored(&self, index: u64, keyframe: bool, slot: Slot, fresh: bool) {
        self.push_entry(
            Entry {
                index,
                keyframe,
                data: store::Data::Stored(slot),
            },
            fresh,
        );
    }

    fn push_entry(&self, entry: Entry, fresh: bool) {
        let bytes = entry.length();
        let (oldest, refused) = {
            let mut ring = self.ring();
            let refused = ring.push(entry).err();
            (ring.first_index(), refused)
        };
        if let Some(error) = refused {
            log::warning(format!(
                "The replay buffer could no longer be kept outside RAM ({error}); it continues in RAM"
            ));
        }
        self.counters.frame(bytes, oldest, fresh);
    }

    pub(crate) fn set_audio(&self, mut format: AudioFormat) {
        format.sample_rate = format.sample_rate.max(1);
        format.channels = format.channels.max(1);
        let _ = self.audio.set(Arc::new(format));
    }

    pub(crate) fn push_audio(&self, start: u64, data: Arc<[i16]>) {
        let Some(format) = self.audio.get() else {
            return;
        };
        let video_start = self.video_time(self.counters.oldest.load(Ordering::Relaxed));
        let mut sound = self.sound();
        sound.push(format, start, data);
        sound.trim(format, video_start);
    }

    fn video_time(&self, index: u64) -> Option<i64> {
        let origin = *self.origin.get()?;
        let frame_rate = self.stream()?.frame_rate;
        (index != u64::MAX).then(|| tick_time(origin, frame_rate, index))
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.counters.bytes.load(Ordering::Relaxed)
    }

    /// Frames encoded, and how many of them were fresh.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (
            self.counters.encoded.load(Ordering::Relaxed),
            self.counters.fresh.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub(crate) fn buffered(&self) -> (u64, usize) {
        let ring = self.ring();
        (ring.frames(), ring.bytes())
    }

    /// Cheap: stored packets are read back, and the sound encoded, only as a save writes them.
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

    fn sound(&self) -> MutexGuard<'_, AudioRing> {
        self.sound.lock().unwrap_or_else(PoisonError::into_inner)
    }
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

#[derive(Debug, Error)]
pub(crate) enum SaveError {
    #[error("{step} {}: {source}", path.display())]
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

/// The `avcC`, `hvcC` or `av1C` record from the sequence header in `data`.
fn configuration(codec: Codec, data: &[u8]) -> Option<Vec<u8>> {
    match codec {
        Codec::H264 => h264::configuration(data),
        Codec::Hevc => hevc::configuration(data),
        Codec::Av1 => av1::configuration(data),
    }
}

fn sample_parts(codec: Codec, data: &[u8]) -> Vec<&[u8]> {
    match codec {
        Codec::H264 => h264::sample_units(data),
        Codec::Hevc => hevc::sample_units(data),
        Codec::Av1 => av1::sample_obus(data),
    }
}

/// Writes `snapshot` to `directory` as `hindsight_<date>_<time>.mp4`; the file appears under
/// its final name only once complete.
pub(crate) fn save(snapshot: &Snapshot, directory: &Path) -> Result<PathBuf, SaveError> {
    fs::create_dir_all(directory).map_err(SaveError::io("create the clip folder", directory))?;
    let path = unused_path(directory, &log::file_stamp());
    write_then_rename(snapshot, &path)?;
    Ok(path)
}

fn write_then_rename(snapshot: &Snapshot, path: &Path) -> Result<(), SaveError> {
    let partial = path.with_extension(PARTIAL_EXTENSION);
    let result = write_file(snapshot, &partial)
        .and_then(|()| fs::rename(&partial, path).map_err(SaveError::io("finish", path)));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_file(snapshot: &Snapshot, path: &Path) -> Result<(), SaveError> {
    let audio = snapshot.audio.as_ref().and_then(encode_sound);
    let file = File::create(path).map_err(SaveError::io("create", path))?;
    let mut output = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    mp4::write_from(
        &mut output,
        &snapshot.stream,
        &snapshot.entries[..],
        audio.as_ref(),
    )?;
    sync(output).map_err(SaveError::io("finish", path))
}

fn encode_sound(pcm: &PcmClip) -> Option<AudioClip> {
    crate::audio::encode(pcm)
        .inspect_err(|error| log::warning(format!("The clip is saved without sound: {error}")))
        .ok()
}

fn sync(output: BufWriter<File>) -> io::Result<()> {
    let file = output
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

fn unused_path(directory: &Path, stamp: &str) -> PathBuf {
    (1..=u32::MAX)
        .map(|attempt| {
            let suffix = if attempt == 1 {
                String::new()
            } else {
                format!("-{attempt}")
            };
            directory.join(format!("{CLIP_PREFIX}_{stamp}{suffix}.{CLIP_EXTENSION}"))
        })
        .find(|path| !path.exists())
        .expect("one of four billion clip names is unused")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;
    use store::Data;

    const STREAM: Stream = Stream {
        codec: Codec::H264,
        width: 1280,
        height: 720,
        frame_rate: 30,
        matrix: Matrix::Bt709,
    };

    /// The real encoder output in `tests/fixtures` is 128×72.
    fn fixture_stream(codec: Codec) -> Stream {
        Stream {
            codec,
            width: 128,
            height: 72,
            ..STREAM
        }
    }

    /// An H.264 packet of at least `bytes`, a keyframe every 30 frames.
    fn h264_packet(index: u64, bytes: usize) -> Packet {
        let keyframe = index.is_multiple_of(30);
        let mut data = if keyframe {
            h264::tests::keyframe()
        } else {
            h264::tests::inter_frame()
        };
        data.resize(data.len().max(bytes), 0xAB);
        Packet {
            index,
            keyframe,
            data: data.into(),
        }
    }

    fn push_frames(live: &Live, frames: u64) {
        for index in 0..frames {
            live.push(h264_packet(index, 0), index % 2 == 0);
        }
    }

    fn live_with(frames: u64) -> Live {
        let live = Live::new(60);
        live.set_stream(STREAM);
        push_frames(&live, frames);
        live
    }

    #[test]
    fn counts_frames_and_snapshots_the_buffer() {
        let live = live_with(100);

        assert_eq!(live.counters(), (100, 50));
        let (frames, bytes) = live.buffered();
        assert_eq!(frames, 70);
        assert!(bytes > 0);

        let clip = live.clip().unwrap();
        assert_eq!(clip.packets.len(), 70);
        assert!(clip.packets[0].keyframe);
        assert!((clip.seconds() - 70.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_buffer_in_a_store_saves_the_same_clip() {
        let in_memory = live_with(100).clip().unwrap();
        let mut expected = Vec::new();
        mp4::write(&mut expected, &in_memory.stream, &in_memory.packets).unwrap();
        for (piece, fail_after) in [(1000, usize::MAX), (7, usize::MAX), (7, 50)] {
            let live = Live::new(60);
            live.set_stream(STREAM);
            let mut memory_store = store::tests::MemoryStore::new(1 << 16, piece);
            memory_store.fail_after = fail_after;
            live.set_store(Box::new(memory_store));
            push_frames(&live, 100);
            let snapshot = live.snapshot().unwrap();
            assert!((snapshot.seconds() - in_memory.seconds()).abs() < 1e-9);
            let kinds: Vec<bool> = snapshot
                .entries
                .iter()
                .map(|entry| matches!(entry.data, Data::Memory(_)))
                .collect();
            assert_eq!(kinds.contains(&true), fail_after != usize::MAX);
            assert!(!kinds[0], "the oldest packets are in the store");

            let directory = tempfile::tempdir().unwrap();
            let path = save(&snapshot, directory.path()).unwrap();
            assert_eq!(fs::read(path).unwrap(), expected, "pieces of {piece}");

            let from_store = snapshot.clip().unwrap();
            assert_eq!(from_store.packets.len(), in_memory.packets.len());
            for (stored, kept) in from_store.packets.iter().zip(&in_memory.packets) {
                assert_eq!(stored.index, kept.index);
                assert_eq!(stored.keyframe, kept.keyframe);
                assert_eq!(stored.data, kept.data);
            }
        }
    }

    #[test]
    fn a_buffer_on_disk_saves_the_same_clip() {
        let push = |live: &Live| {
            for index in 0..100 {
                let bytes = 200 + 5 * usize::try_from(index).unwrap();
                live.push(h264_packet(index, bytes), true);
            }
        };
        let in_memory = Live::new(60);
        in_memory.set_stream(STREAM);
        push(&in_memory);
        let in_memory = in_memory.clip().unwrap();
        let mut expected = Vec::new();
        mp4::write(&mut expected, &in_memory.stream, &in_memory.packets).unwrap();

        let folder = tempfile::tempdir().unwrap();
        let store = disk::DiskStore::small(folder.path(), 4);
        let control = store.control();
        control.hold(true);
        let live = Live::new(60);
        live.set_stream(STREAM);
        live.set_store(Box::new(store));
        push(&live);
        let snapshot = live.snapshot().unwrap();
        assert!(
            snapshot
                .entries
                .iter()
                .all(|entry| matches!(entry.data, Data::Stored(_)))
        );
        assert!(control.queued() > 0);

        let clips = tempfile::tempdir().unwrap();
        let path = save(&snapshot, clips.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected, "from staging");
        fs::remove_file(path).unwrap();

        control.hold(false);
        control.settle();
        let path = save(&snapshot, clips.path()).unwrap();
        assert_eq!(fs::read(path).unwrap(), expected, "from the file");
    }

    #[test]
    fn a_save_from_a_store_holds_a_few_packets_not_the_clip() {
        const FRAMES: usize = 120;
        const FRAME_BYTES: usize = 256 << 10;
        let live = Live::new(FRAMES as u64);
        live.set_stream(STREAM);
        live.set_store(Box::new(store::tests::MemoryStore::new(4 << 20, 1 << 20)));
        for (index, extra) in (0..).zip(0..FRAMES) {
            live.push(h264_packet(index, FRAME_BYTES + extra), true);
        }
        let snapshot = live.snapshot().unwrap();
        let clip_bytes = FRAMES * FRAME_BYTES;
        let directory = tempfile::tempdir().unwrap();

        let saving = heap::peak_during(|| {
            save(&snapshot, directory.path()).unwrap();
        });
        assert!(
            saving < WRITE_BUFFER_BYTES + 2 * FRAME_BYTES + (256 << 10),
            "a save held {saving} bytes at once"
        );
        let reading_all = heap::peak_during(|| drop(snapshot.clip().unwrap()));
        assert!(reading_all >= clip_bytes, "{reading_all} < {clip_bytes}");
    }

    #[test]
    fn keyframes_are_recognised_for_every_codec() {
        assert!(is_keyframe(Codec::H264, &h264::tests::keyframe()));
        assert!(!is_keyframe(Codec::H264, &h264::tests::inter_frame()));
        assert!(is_keyframe(Codec::Hevc, &hevc::tests::keyframe()));
        assert!(!is_keyframe(Codec::Hevc, &hevc::tests::inter_frame()));
        assert!(is_keyframe(Codec::Av1, &av1::tests::keyframe()));
        assert!(!is_keyframe(Codec::Av1, &av1::tests::inter_frame()));
    }

    fn encoded_fixture(codec: Codec) -> Vec<Packet> {
        let units: Vec<&[u8]> = match codec {
            Codec::H264 => {
                annexb::access_units(include_bytes!("../tests/fixtures/h264.264"), |unit| {
                    h264::nal_type(unit) == h264::NAL_AUD
                })
            }
            Codec::Hevc => {
                annexb::access_units(include_bytes!("../tests/fixtures/hevc.265"), |unit| {
                    hevc::nal_type(unit) == hevc::NAL_AUD
                })
            }
            Codec::Av1 => av1::temporal_units(include_bytes!("../tests/fixtures/av1.obu")),
        };
        units
            .into_iter()
            .enumerate()
            .map(|(index, data)| Packet {
                index: index as u64,
                keyframe: is_keyframe(codec, data),
                data: data.into(),
            })
            .collect()
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    #[test]
    fn real_encoder_output_muxes_for_every_codec() {
        for codec in Codec::ALL {
            let packets = encoded_fixture(codec);
            assert_eq!(packets.len(), 75, "{codec}");
            let keyframes: Vec<u64> = packets
                .iter()
                .filter(|packet| packet.keyframe)
                .map(|packet| packet.index)
                .collect();
            assert_eq!(keyframes, [0, 30, 60], "{codec}");

            let mut ring = Ring::new(40);
            for packet in packets {
                ring.push(packet.into()).unwrap();
            }
            let clip = Clip {
                stream: fixture_stream(codec),
                packets: store::fetch(&ring.snapshot()).unwrap(),
                audio: None,
            };
            assert_eq!(clip.packets.len(), 45, "{codec}");

            let mut file = Vec::new();
            mp4::write(&mut file, &clip.stream, &clip.packets).unwrap();
            let top = mp4::tests::atoms(&file);
            let kinds: Vec<_> = top.iter().map(|(kind, _)| kind).collect();
            assert_eq!(kinds, [b"ftyp", b"moov", b"mdat"], "{codec}");

            let (moov, media) = mp4::tests::index(&file);
            assert_eq!(
                (moov.mvhd.timescale, moov.mvhd.duration),
                (1000, 1500),
                "{codec}"
            );
            let [track] = &moov.trak[..] else {
                panic!("{codec}: expected one track");
            };
            assert_eq!(track.tkhd.track_id, 1, "{codec}");
            assert_eq!(
                (track.mdia.mdhd.timescale, track.mdia.mdhd.duration),
                (30_000, 45_000),
                "{codec}"
            );
            let table = &track.mdia.minf.stbl;
            assert_eq!(table.stss.as_ref().unwrap().entries, [1, 31], "{codec}");
            assert_eq!(table.stts.entries.len(), 1, "{codec}");

            let samples: Vec<_> = clip
                .packets
                .iter()
                .map(|packet| mp4::tests::stored(codec, packet))
                .collect();
            let sample_sizes: Vec<_> = samples
                .iter()
                .map(|sample| u32::try_from(sample.len()).unwrap())
                .collect();
            assert_eq!(mp4::tests::sizes(table), sample_sizes, "{codec}");
            let [(offset, bytes)] = &mp4::tests::chunks(table, &samples)[..] else {
                panic!("{codec}: expected one chunk");
            };
            assert_eq!(*offset, media, "{codec}");
            assert_eq!(&file[*offset..], &bytes[..], "{codec}");
            let expected_media = match codec {
                Codec::H264 => 0x4CC2_8686_4D7A_C7C8,
                Codec::Hevc => 0x1944_D923_E869_BE8C,
                Codec::Av1 => 0x6028_8448_497C_A88C,
            };
            assert_eq!(fnv1a(top[2].1), expected_media, "{codec}");

            let record = mp4::tests::decoder_record(track);
            assert_eq!(
                record,
                configuration(codec, &clip.packets[0].data).unwrap(),
                "{codec}"
            );
            let expected_record = match codec {
                Codec::H264 => 0x5B56_C447_D3ED_352B,
                Codec::Hevc => 0x716B_345E_8965_10BA,
                Codec::Av1 => 0xDAD6_9D69_8A69_A58C,
            };
            assert_eq!(fnv1a(&record), expected_record, "{codec}");
            if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
                let name = format!("{}.mp4", codec.name().replace('.', "").to_lowercase());
                fs::write(Path::new(&directory).join(name), &file).unwrap();
            }
        }
    }

    #[test]
    fn real_encoder_output_muxes_with_sound() {
        let live = Live::new(40);
        live.set_stream(fixture_stream(Codec::H264));
        live.set_origin(0);
        let frames = audio::tests::encoded_fixture();
        assert_eq!(frames.len(), 119);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            anchor: 0,
            priming: FRAME_SAMPLES,
        });
        for packet in encoded_fixture(Codec::H264) {
            live.push(packet, true);
        }
        live.push_audio(0, audio::tests::numbered(0, 120_000));

        let clip = live.clip().unwrap();
        assert_eq!(clip.packets.len(), 45);
        let pcm = clip.audio.as_ref().unwrap();
        assert_eq!(pcm.start, 46 * 1024);
        assert_eq!(pcm.frames, 119 - 46);
        assert_eq!(pcm.lead, 49_024 - 46 * 1024);
        let audio = &audio::tests::aac_clip(frames[46..].to_vec(), pcm.lead);

        let mut file = Vec::new();
        mp4::write_with_audio(&mut file, &clip.stream, &clip.packets, audio).unwrap();
        if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
            fs::write(Path::new(&directory).join("h264-aac.mp4"), &file).unwrap();
        }

        let (moov, media) = mp4::tests::index(&file);
        assert_eq!((moov.mvhd.duration, moov.mvhd.next_track_id), (1500, 3));
        let [video, sound] = &moov.trak[..] else {
            panic!("expected two tracks");
        };
        assert_eq!((video.tkhd.track_id, sound.tkhd.track_id), (1, 2));
        assert_eq!(
            (sound.mdia.mdhd.timescale, sound.mdia.mdhd.duration),
            (48_000, 73 * 1024)
        );
        let edits = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries;
        assert_eq!(edits.len(), 1);
        assert_eq!(
            (edits[0].segment_duration, edits[0].media_time),
            (1500, Some(1920))
        );
        let sound_table = &sound.mdia.minf.stbl;
        assert!(sound_table.stss.is_none());
        let frame_sizes: Vec<_> = audio
            .frames
            .iter()
            .map(|frame| u32::try_from(frame.len()).unwrap())
            .collect();
        assert_eq!(mp4::tests::sizes(sound_table), frame_sizes);

        let video_samples: Vec<_> = clip
            .packets
            .iter()
            .map(|packet| mp4::tests::stored(Codec::H264, packet))
            .collect();
        let sound_samples: Vec<_> = audio.frames.iter().map(|frame| frame.to_vec()).collect();
        let tagged = |table, samples, is_video| {
            mp4::tests::chunks(table, samples)
                .into_iter()
                .map(move |(offset, bytes)| (offset, bytes, is_video))
        };
        let mut all: Vec<_> = tagged(&video.mdia.minf.stbl, &video_samples, true)
            .chain(tagged(sound_table, &sound_samples, false))
            .collect();
        all.sort_by_key(|(offset, _, _)| *offset);
        let order: Vec<_> = all.iter().map(|(_, _, is_video)| *is_video).collect();
        assert_eq!(
            order,
            [true, false, true, false],
            "video, sound, video, sound"
        );
        let mut next = media;
        for (offset, bytes, _) in all {
            assert_eq!(offset, next);
            assert_eq!(&file[offset..offset + bytes.len()], &bytes[..]);
            next += bytes.len();
        }
        assert_eq!(next, file.len());
        assert_eq!(fnv1a(mp4::tests::atoms(&file)[2].1), 0x56DC_4C5D_809C_3A4E);
        assert_eq!(
            fnv1a(&mp4::tests::decoder_record(video)),
            0x5B56_C447_D3ED_352B
        );
    }

    #[test]
    fn the_sound_is_trimmed_with_the_video_and_cut_to_it() {
        let live = Live::new(60);
        live.set_stream(STREAM);
        live.set_origin(0);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            anchor: 0,
            priming: 0,
        });
        let read: u64 = 2880;
        let mut next_sample: u64 = 0;
        for index in 0..150 {
            live.push(h264_packet(index, 0), true);
            let now = tick_time(0, 30, index);
            while i64::try_from((next_sample + read) * 10_000_000 / 48_000).unwrap() <= now {
                live.push_audio(next_sample, audio::tests::numbered(next_sample, read));
                next_sample += read;
            }
        }

        let clip = live.clip().unwrap();
        assert_eq!(clip.packets[0].index, 90);
        let audio = clip.audio.unwrap();
        assert_eq!(audio.start, 139 * 1024);
        assert_eq!(audio.lead, 144_000 - 139 * 1024);
        assert_eq!(audio.frames, 235 - 139);
        let mut samples = [7; 4];
        audio.read(139 * 1024, &mut samples);
        assert_eq!(samples, [22_336, -22_336, 22_337, -22_337]);
        audio.read(239_000, &mut samples);
        assert_eq!(samples, [0; 4]);

        live.push_audio(next_sample, audio::tests::numbered(next_sample, read));
        let format = live.audio.get().unwrap();
        assert_eq!(live.sound().runs(format)[0].0, 139 * 1024);
    }

    #[cfg(windows)]
    fn push_sound(live: &Live, start: u64, end: u64, sample: impl Fn(u64) -> i16) {
        let mut at = start;
        while at < end {
            let next = (at + 2880).min(end);
            let data: Arc<[i16]> = (at..next)
                .flat_map(|number| {
                    let value = sample(number);
                    [value, value]
                })
                .collect();
            live.push_audio(at, data);
            at = next;
        }
    }

    #[cfg(windows)]
    fn live_with_a_click() -> Live {
        let live = live_with(90);
        live.set_origin(0);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            anchor: CLOCK_RATE / 3,
            priming: 0,
        });
        let click = |number| if number == 80_000 { 30_000 } else { 0 };
        push_sound(&live, 0, 104_000, click);
        push_sound(&live, 116_000, 137_600, click);
        live
    }

    #[cfg(windows)]
    fn snapshot_with_sound(live: &Live) -> Option<Snapshot> {
        let snapshot = live.snapshot().unwrap();
        match crate::audio::encode(snapshot.audio.as_ref().unwrap()) {
            Ok(_) => Some(snapshot),
            Err(error) => {
                eprintln!("skipped: {error}");
                None
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn a_saved_clip_encodes_its_sound_in_line_with_the_video() {
        let live = live_with_a_click();
        let Some(snapshot) = snapshot_with_sound(&live) else {
            return;
        };
        let pcm = snapshot.audio.as_ref().unwrap();
        assert_eq!(
            (pcm.start, pcm.frames, pcm.lead),
            (30 * 1024, 125 - 30, 32_000 - 30 * 1024)
        );

        let directory = tempfile::tempdir().unwrap();
        let file = fs::read(save(&snapshot, directory.path()).unwrap()).unwrap();
        let (moov, _) = mp4::tests::index(&file);
        let [_, sound] = &moov.trak[..] else {
            panic!("expected two tracks");
        };
        assert_eq!(moov.mvhd.duration, 2000);
        assert_eq!(
            (sound.mdia.mdhd.timescale, sound.mdia.mdhd.duration),
            (48_000, 95 * 1024)
        );
        let edits = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries;
        assert_eq!(
            edits
                .iter()
                .map(|edit| (edit.segment_duration, edit.media_time))
                .collect::<Vec<_>>(),
            [(2000, Some(1280))]
        );
        let sizes = mp4::tests::sizes(&sound.mdia.minf.stbl);
        assert_eq!(sizes.len(), 95);
        let bytes: u32 = sizes.iter().sum();
        assert!((40_000..=56_000).contains(&bytes), "{bytes} bytes");
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "needs ffmpeg"]
    fn the_click_plays_in_time_with_the_video() {
        let live = live_with_a_click();
        let snapshot = snapshot_with_sound(&live).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let clip = save(&snapshot, directory.path()).unwrap();
        let decoded = crate::audio::testing::decode_mono(&clip);
        let peak = crate::audio::testing::peak(&decoded);
        println!(
            "{} samples decoded for 2 s of video, the click at {peak}",
            decoded.len()
        );
        assert_eq!(decoded.len(), 96_000);
        assert!(peak.abs_diff(48_000) <= 2, "the click at {peak}");
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "a timing, not a check"]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the signal stays within ±12 000, and sample numbers are far below 2^52"
    )]
    fn encodes_five_minutes_of_sound() {
        let live = Live::new(9000);
        live.set_stream(STREAM);
        push_frames(&live, 9000);
        live.set_origin(0);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            anchor: 0,
            priming: 0,
        });
        let mut seed: u32 = 1;
        let noise: Vec<i16> = (0..48_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                i16::try_from(seed >> 20).unwrap() - 2048
            })
            .collect();
        push_sound(&live, 0, 300 * 48_000, |number| {
            let time = number as f64 / 48_000.0;
            let chord = (time * 2764.6).sin() * 5000.0 + (time * 3487.2).sin() * 4000.0;
            chord as i16 + noise[usize::try_from(number % 48_000).unwrap()]
        });
        let snapshot = live.snapshot().unwrap();
        let pcm = snapshot.audio.as_ref().unwrap();
        let start = std::time::Instant::now();
        let audio = crate::audio::encode(pcm).unwrap();
        println!(
            "{} frames ({} bytes) of AAC from 300 s of sound in {:?}",
            audio.frames.len(),
            audio.frames.iter().map(|frame| frame.len()).sum::<usize>(),
            start.elapsed()
        );
    }

    #[test]
    fn nothing_to_save_before_the_stream_starts() {
        let live = Live::new(10);
        assert!(live.clip().is_none());
        live.set_stream(STREAM);
        assert!(live.clip().is_none());
    }

    #[test]
    fn saves_complete_files_under_unique_names() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = live_with(40).snapshot().unwrap();
        let target = directory.path().join("clips");

        let first = save(&snapshot, &target).unwrap();
        let second = save(&snapshot, &target).unwrap();

        assert_ne!(first, second);
        assert!(
            first
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("hindsight_")
        );
        assert_eq!(first.extension().unwrap(), "mp4");
        assert_eq!(&fs::read(&first).unwrap()[4..8], b"ftyp");
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 2);
    }

    #[test]
    fn a_failed_save_leaves_no_partial_file() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = Snapshot {
            stream: STREAM,
            entries: vec![Arc::new(h264_packet(1, 0).into())],
            audio: None,
        };

        assert_matches!(
            save(&snapshot, directory.path()),
            Err(SaveError::Mux(MuxError::NoSequenceHeader))
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[expect(unsafe_code)]
    mod heap {
        use std::{
            alloc::{GlobalAlloc, Layout, System},
            cell::Cell,
        };

        thread_local! {
            static HELD: Cell<isize> = const { Cell::new(0) };
            static PEAK: Cell<isize> = const { Cell::new(0) };
        }

        struct Counting;

        // SAFETY: every call goes to the system allocator unchanged; counting touches no memory.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                // SAFETY: the caller's promises about `layout` are the system allocator's.
                let pointer = unsafe { System.alloc(layout) };
                if !pointer.is_null() {
                    count(size(layout.size()));
                }
                pointer
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                // SAFETY: as for `alloc`.
                let pointer = unsafe { System.alloc_zeroed(layout) };
                if !pointer.is_null() {
                    count(size(layout.size()));
                }
                pointer
            }

            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                // SAFETY: the caller allocated `pointer` with `layout` in the system allocator.
                unsafe { System.dealloc(pointer, layout) };
                count(-size(layout.size()));
            }

            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                // SAFETY: as for `dealloc`; the caller's promises about `new_size` pass through.
                let moved = unsafe { System.realloc(pointer, layout, new_size) };
                if !moved.is_null() {
                    count(size(new_size) - size(layout.size()));
                }
                moved
            }
        }

        #[global_allocator]
        static COUNTING: Counting = Counting;

        fn size(bytes: usize) -> isize {
            isize::try_from(bytes).unwrap_or(isize::MAX)
        }

        fn count(bytes: isize) {
            let _ = HELD.try_with(|held| {
                let now = held.get() + bytes;
                held.set(now);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
            });
        }

        pub(super) fn peak_during(work: impl FnOnce()) -> usize {
            let before = HELD.with(Cell::get);
            PEAK.with(|peak| peak.set(before));
            work();
            usize::try_from(PEAK.with(Cell::get) - before).unwrap_or(0)
        }
    }
}
