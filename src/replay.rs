//! What a recording session shares with the rest of the application: the rolling buffers of
//! encoded video packets and sound frames, the streams they belong to, and counters for the
//! Status tab. Saving takes a snapshot of the buffers and writes it to an MP4 file while
//! recording carries on.

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
mod h264;
mod hevc;
mod mp4;
mod ring;

pub(crate) use audio::{AudioClip, AudioFormat, AudioFrame, CLOCK_RATE, FRAME_SAMPLES};
use audio::{AudioRing, tick_time};
use mp4::MuxError;
use ring::Ring;

const CLIP_PREFIX: &str = "hindsight";
const CLIP_EXTENSION: &str = "mp4";
const PARTIAL_EXTENSION: &str = "partial";
const WRITE_BUFFER_BYTES: usize = 1 << 20;

/// One encoded frame: an H.264 or HEVC access unit in Annex B form, or an AV1 temporal unit.
#[derive(Debug)]
pub(crate) struct Packet {
    /// The frame's tick of the session's frame clock; skipped ticks leave gaps.
    pub(crate) index: u64,
    pub(crate) keyframe: bool,
    pub(crate) data: Box<[u8]>,
}

/// The encoded video's shape, fixed for a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stream {
    pub(crate) codec: Codec,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_rate: u32,
}

/// A session's buffer and counters, shared between the capture thread and the worker.
pub(crate) struct Live {
    ring: Mutex<Ring>,
    stream: OnceLock<Stream>,
    /// Frames encoded, including repeats of an unchanged screen.
    encoded: AtomicU64,
    /// Frames that brought new screen content.
    fresh: AtomicU64,
    /// Encoded bytes that went into the buffer, including those since dropped from it.
    bytes: AtomicU64,
    /// Called once the stream is recorded, so the owner learns of it without polling.
    on_stream: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// The clock time of the frame clock's first tick, once the capture has started; see
    /// [`audio`] for the clock both streams share.
    origin: OnceLock<i64>,
    /// The frame index of the oldest buffered packet, or `u64::MAX` before the first; the sound
    /// is trimmed to it.
    oldest: AtomicU64,
    /// The encoded sound's shape, once the audio capture has started.
    audio: OnceLock<Arc<AudioFormat>>,
    sound: Mutex<AudioRing>,
}

impl Live {
    /// An empty buffer that keeps at least `span` frames.
    pub(crate) fn new(span: u64) -> Self {
        Self {
            ring: Mutex::new(Ring::new(span)),
            stream: OnceLock::new(),
            encoded: AtomicU64::new(0),
            fresh: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            on_stream: OnceLock::new(),
            origin: OnceLock::new(),
            oldest: AtomicU64::new(u64::MAX),
            audio: OnceLock::new(),
            sound: Mutex::new(AudioRing::default()),
        }
    }

    /// Runs `callback` when the stream is recorded; it runs on the capture thread.
    pub(crate) fn on_stream(&self, callback: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_stream.set(Box::new(callback));
    }

    /// Records the stream's shape once the encoder is configured.
    pub(crate) fn set_stream(&self, stream: Stream) {
        if self.stream.set(stream).is_ok()
            && let Some(callback) = self.on_stream.get()
        {
            callback();
        }
    }

    /// The stream's shape, once it is recorded.
    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream.get().copied()
    }

    /// Records the clock time (QPC, in 100 ns units) of the frame clock's first tick, which
    /// places every packet's frame index on the clock the sound is stamped with.
    pub(crate) fn set_origin(&self, time: i64) {
        let _ = self.origin.set(time);
    }

    /// Adds an encoded frame; `fresh` when it carried new screen content.
    pub(crate) fn push(&self, packet: Packet, fresh: bool) {
        self.bytes
            .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
        let oldest = {
            let mut ring = self.ring();
            ring.push(packet);
            ring.first_index()
        };
        self.oldest
            .store(oldest.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.encoded.fetch_add(1, Ordering::Relaxed);
        if fresh {
            self.fresh.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records the encoded sound's shape once the audio encoder is configured.
    pub(crate) fn set_audio(&self, format: AudioFormat) {
        let _ = self.audio.set(Arc::new(format));
    }

    /// Adds an encoded sound frame, then drops the frames that no clip of the buffered video can
    /// need.
    pub(crate) fn push_audio(&self, frame: AudioFrame) {
        let Some(format) = self.audio.get() else {
            return;
        };
        self.bytes
            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        let start = self.video_time(self.oldest.load(Ordering::Relaxed));
        let mut sound = self.sound();
        sound.push(frame);
        sound.trim(format, start);
    }

    /// The clock time of video frame `index`, once the clock and the frame rate are known;
    /// `u64::MAX` stands for no frame.
    fn video_time(&self, index: u64) -> Option<i64> {
        let origin = *self.origin.get()?;
        let frame_rate = self.stream()?.frame_rate;
        (index != u64::MAX).then(|| tick_time(origin, frame_rate, index))
    }

    /// Encoded bytes that went into the buffer since the session started.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Frames encoded and frames with new content, since the session started.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (
            self.encoded.load(Ordering::Relaxed),
            self.fresh.load(Ordering::Relaxed),
        )
    }

    /// Frames and bytes currently buffered.
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> (u64, usize) {
        let ring = self.ring();
        (ring.frames(), ring.bytes())
    }

    /// The buffer as it is now, if the stream has started, with the sound from the first
    /// packet's frame to the end of the last one's when there is sound.
    pub(crate) fn clip(&self) -> Option<Clip> {
        let stream = self.stream()?;
        let packets = self.ring().snapshot();
        let (first, last) = (packets.first()?.index, packets.last()?.index);
        let audio = self.audio_between(first, last + 1);
        Some(Clip {
            stream,
            packets,
            audio,
        })
    }

    /// The sound from the start of video frame `first` to the start of frame `end`.
    fn audio_between(&self, first: u64, end: u64) -> Option<AudioClip> {
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

/// What a save writes: the stream and the packets buffered when it was requested, and the sound
/// that plays alongside them.
pub(crate) struct Clip {
    pub(crate) stream: Stream,
    pub(crate) packets: Vec<Arc<Packet>>,
    pub(crate) audio: Option<AudioClip>,
}

impl Clip {
    /// Seconds of video, from the first packet to the end of the last.
    pub(crate) fn seconds(&self) -> f64 {
        match (self.packets.first(), self.packets.last()) {
            (Some(first), Some(last)) => {
                (last.index - first.index + 1) as f64 / f64::from(self.stream.frame_rate)
            }
            _ => 0.0,
        }
    }
}

/// Why a clip could not be saved.
#[derive(Debug, Error)]
pub(crate) enum SaveError {
    /// A file system call failed; `step` says which, in words for the log.
    #[error("{step} {}: {source}", path.display())]
    Io {
        step: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The clip could not be muxed, or the muxed bytes could not be written.
    #[error(transparent)]
    Mux(#[from] MuxError),
}

impl SaveError {
    /// Wraps the error of the file system call `step` made on `path`, for `map_err`.
    fn io(step: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io {
            step,
            path: path.to_owned(),
            source,
        }
    }
}

/// Whether `data` starts a group of pictures a decoder can begin with.
pub(crate) fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::H264 => h264::is_keyframe(data),
        Codec::Hevc => hevc::is_keyframe(data),
        Codec::Av1 => av1::is_keyframe(data),
    }
}

/// The decoder configuration record (`avcC`, `hvcC` or `av1C`) from the sequence header in
/// `data`, if it carries one.
fn configuration(codec: Codec, data: &[u8]) -> Option<Vec<u8>> {
    match codec {
        Codec::H264 => h264::configuration(data),
        Codec::Hevc => hevc::configuration(data),
        Codec::Av1 => av1::configuration(data),
    }
}

/// The pieces of `data` a stored sample keeps: NAL units, or whole OBUs.
fn sample_parts(codec: Codec, data: &[u8]) -> Vec<&[u8]> {
    match codec {
        Codec::H264 => h264::sample_units(data),
        Codec::Hevc => hevc::sample_units(data),
        Codec::Av1 => av1::sample_obus(data),
    }
}

/// Writes `clip` into `directory` as `hindsight_<date>_<time>.mp4` and returns its path. The file
/// appears under its final name only once it is complete.
pub(crate) fn save(clip: &Clip, directory: &Path) -> Result<PathBuf, SaveError> {
    fs::create_dir_all(directory).map_err(SaveError::io("create the clip folder", directory))?;
    let path = unused_path(directory, &log::file_stamp());
    write_then_rename(clip, &path)?;
    Ok(path)
}

/// Writes `clip` next to `path` under a partial name and renames it to `path` once complete,
/// removing the partial file if either step fails.
fn write_then_rename(clip: &Clip, path: &Path) -> Result<(), SaveError> {
    let partial = path.with_extension(PARTIAL_EXTENSION);
    let result = write_file(clip, &partial)
        .and_then(|()| fs::rename(&partial, path).map_err(SaveError::io("finish", path)));
    if result.is_err() {
        // The save already failed; a leftover partial file is all that is lost here.
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Muxes `clip` into a new file at `path` and waits until it is on the disk.
fn write_file(clip: &Clip, path: &Path) -> Result<(), SaveError> {
    let file = File::create(path).map_err(SaveError::io("create", path))?;
    let mut output = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    match &clip.audio {
        Some(audio) => mp4::write_with_audio(&mut output, &clip.stream, &clip.packets, audio)?,
        None => mp4::write(&mut output, &clip.stream, &clip.packets)?,
    }
    sync(output).map_err(SaveError::io("finish", path))
}

/// Flushes `output`'s buffer into its file, then the file's contents to the disk.
fn sync(output: BufWriter<File>) -> io::Result<()> {
    let file = output
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// `hindsight_<stamp>.mp4`, or with `-2`, `-3`, … when a clip was already saved that second.
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
    use h264::tests::{inter_frame, keyframe};
    use std::assert_matches;

    const STREAM: Stream = Stream {
        codec: Codec::H264,
        width: 1280,
        height: 720,
        frame_rate: 30,
    };

    fn live_with(frames: u64) -> Live {
        let live = Live::new(60);
        live.set_stream(STREAM);
        for index in 0..frames {
            let keyframe_packet = index % 30 == 0;
            let data = if keyframe_packet {
                keyframe()
            } else {
                inter_frame()
            };
            live.push(
                Packet {
                    index,
                    keyframe: keyframe_packet,
                    data: data.into(),
                },
                index % 2 == 0,
            );
        }
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
    fn keyframes_are_recognised_for_every_codec() {
        assert!(is_keyframe(Codec::H264, &h264::tests::keyframe()));
        assert!(!is_keyframe(Codec::H264, &h264::tests::inter_frame()));
        assert!(is_keyframe(Codec::Hevc, &hevc::tests::keyframe()));
        assert!(!is_keyframe(Codec::Hevc, &hevc::tests::inter_frame()));
        assert!(is_keyframe(Codec::Av1, &av1::tests::keyframe()));
        assert!(!is_keyframe(Codec::Av1, &av1::tests::inter_frame()));
    }

    /// Encoder output from x264, x265 and libaom (128×72, 75 frames, a keyframe every 30),
    /// framed like NVENC's: delimiters, and parameter sets repeated on every keyframe.
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

    /// The 64-bit FNV-1a hash of `bytes`: stable across Rust releases, unlike `DefaultHasher`.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    /// Set `HINDSIGHT_MUX_OUTPUT` to a directory to keep the files, e.g. to check them with ffprobe.
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
                ring.push(packet);
            }
            let clip = Clip {
                stream: Stream {
                    codec,
                    width: 128,
                    height: 72,
                    frame_rate: 30,
                },
                packets: ring.snapshot(),
                audio: None,
            };
            assert_eq!(clip.packets.len(), 45, "{codec}");

            let mut file = Vec::new();
            mp4::write(&mut file, &clip.stream, &clip.packets).unwrap();
            let top = mp4::tests::atoms(&file);
            assert_eq!(top.len(), 3, "{codec}");
            // Pins the written bytes, so a refactor of the writer shows up as a changed file.
            let expected = match codec {
                Codec::H264 => 0xDD34_1F2C_9F9F_EB0C,
                Codec::Hevc => 0x8B6A_2560_120A_255F,
                Codec::Av1 => 0x3BED_D0D1_7FCC_6739,
            };
            assert_eq!(fnv1a(&file), expected, "{codec}");
            if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
                let name = format!("{}.mp4", codec.name().replace('.', "").to_lowercase());
                fs::write(Path::new(&directory).join(name), &file).unwrap();
            }
        }
    }

    /// Real H.264 and AAC encoder output through the buffer and the writer: the video's 1 s to
    /// 2.5 s mark, at 30 fps, with the sound cut to the same span on the shared clock. Set
    /// `HINDSIGHT_MUX_OUTPUT` to keep the file, e.g. to check both streams with ffprobe.
    #[test]
    fn real_encoder_output_muxes_with_sound() {
        let live = Live::new(40);
        live.set_stream(Stream {
            codec: Codec::H264,
            width: 128,
            height: 72,
            frame_rate: 30,
        });
        live.set_origin(0);
        let frames = audio::tests::encoded_fixture();
        assert_eq!(frames.len(), 119);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            config: Box::new([0x11, 0x90]),
            anchor: 0,
            // ffmpeg's encoder, like most, puts one frame in front of the sound.
            priming: FRAME_SAMPLES,
            silence: Arc::clone(&frames[0]),
        });
        for packet in encoded_fixture(Codec::H264) {
            live.push(packet, true);
        }
        for (number, data) in (0..).zip(frames) {
            live.push_audio(AudioFrame { number, data });
        }

        let clip = live.clip().unwrap();
        assert_eq!(clip.packets.len(), 45);
        let audio = clip.audio.as_ref().unwrap();
        // 1 s is output sample 49 024, in frame 47; frame 46 comes first, and 2.5 s (output
        // sample 121 024) is in frame 118.
        assert_eq!(audio.frames.len(), 119 - 46);
        assert_eq!(audio.lead, 49_024 - 46 * 1024);

        let mut file = Vec::new();
        mp4::write_with_audio(&mut file, &clip.stream, &clip.packets, audio).unwrap();
        if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
            fs::write(Path::new(&directory).join("h264-aac.mp4"), &file).unwrap();
        }
        assert_eq!(fnv1a(&file), 0x5043_654E_42DB_C181);
    }

    #[test]
    fn the_sound_is_trimmed_with_the_video_and_cut_to_it() {
        let live = Live::new(60);
        live.set_stream(STREAM);
        live.set_origin(0);
        live.set_audio(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            config: Box::new([0x11, 0x90]),
            anchor: 0,
            priming: 0,
            silence: Arc::from(&b"silence"[..]),
        });
        // Five seconds of 30 fps video with a keyframe a second, and the sound arriving
        // alongside it, each frame once its start has been reached.
        let mut next_frame: u64 = 0;
        for index in 0..150 {
            let key = index % 30 == 0;
            let data = if key { keyframe() } else { inter_frame() };
            live.push(
                Packet {
                    index,
                    keyframe: key,
                    data: data.into(),
                },
                true,
            );
            let now = tick_time(0, 30, index);
            while i64::try_from(next_frame * 1024 * 10_000_000 / 48_000).unwrap() < now {
                live.push_audio(AudioFrame {
                    number: next_frame,
                    data: Arc::from(&[1, 2, 3][..]),
                });
                next_frame += 1;
            }
        }

        // The video keeps its last two seconds from the keyframe at 3 s; the sound keeps the
        // frame before the one playing then (3 s is sample 144 000, in frame 140), and no more.
        let clip = live.clip().unwrap();
        assert_eq!(clip.packets[0].index, 90);
        assert_eq!(live.sound().numbers()[0], 139);
        let audio = clip.audio.unwrap();
        assert_eq!(audio.lead, 144_000 - 139 * 1024);
        // The video ends at 5 s, sample 240 000, in frame 234; the frames not yet captured
        // when the clip was taken are silence.
        assert_eq!(audio.frames.len(), 235 - 139);
        assert_eq!(&*audio.frames[0], &[1, 2, 3]);
        assert_eq!(&*audio.frames[233 - 139], b"silence");
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
        let clip = live_with(40).clip().unwrap();
        let target = directory.path().join("clips");

        let first = save(&clip, &target).unwrap();
        let second = save(&clip, &target).unwrap();

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
        let clip = Clip {
            stream: STREAM,
            packets: vec![Arc::new(Packet {
                index: 0,
                keyframe: false,
                data: inter_frame().into(),
            })],
            audio: None,
        };

        assert_matches!(
            save(&clip, directory.path()),
            Err(SaveError::Mux(MuxError::NoSequenceHeader))
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
