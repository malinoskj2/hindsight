//! What a recording session shares with the rest of the application: the rolling buffer of
//! encoded packets, the stream they belong to, and counters for the Status tab. Saving takes a
//! snapshot of the buffer and writes it to an MP4 file while recording carries on.

use crate::{config::Codec, log};
use std::{
    fs::{self, File},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

mod annexb;
mod av1;
mod h264;
mod hevc;
mod mp4;
mod ring;

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
}

/// What a save writes: the stream and the packets buffered when it was requested.
pub(crate) struct Clip {
    pub(crate) stream: Stream,
    pub(crate) packets: Vec<Arc<Packet>>,
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

impl Live {
    /// An empty buffer that keeps at least `span` frames.
    pub(crate) fn new(span: u64) -> Self {
        Self {
            ring: Mutex::new(Ring::new(span)),
            stream: OnceLock::new(),
            encoded: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            fresh: AtomicU64::new(0),
            on_stream: OnceLock::new(),
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

    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream.get().copied()
    }

    /// Adds an encoded frame; `fresh` when it carried new screen content.
    pub(crate) fn push(&self, packet: Packet, fresh: bool) {
        self.bytes
            .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
        self.ring().push(packet);
        self.encoded.fetch_add(1, Ordering::Relaxed);
        if fresh {
            self.fresh.fetch_add(1, Ordering::Relaxed);
        }
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

    /// The buffer as it is now, if the stream has started.
    pub(crate) fn clip(&self) -> Option<Clip> {
        let stream = self.stream()?;
        let packets = self.ring().snapshot();
        (!packets.is_empty()).then_some(Clip { stream, packets })
    }

    fn ring(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
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
    #[error(transparent)]
    Mux(#[from] MuxError),
}

/// Writes `clip` into `directory` as `hindsight_<date>_<time>.mp4` and returns its path. The file
/// appears under its final name only once it is complete.
pub(crate) fn save(clip: &Clip, directory: &Path) -> Result<PathBuf, SaveError> {
    fs::create_dir_all(directory).map_err(|source| SaveError::Io {
        step: "create the clip folder",
        path: directory.to_owned(),
        source,
    })?;
    let path = unused_path(directory, &log::file_stamp());
    let partial = path.with_extension(PARTIAL_EXTENSION);

    let result = write_file(clip, &partial).and_then(|()| {
        fs::rename(&partial, &path).map_err(|source| SaveError::Io {
            step: "finish",
            path: path.clone(),
            source,
        })
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(|()| path)
}

fn write_file(clip: &Clip, path: &Path) -> Result<(), SaveError> {
    let file = File::create(path).map_err(|source| SaveError::Io {
        step: "create",
        path: path.to_owned(),
        source,
    })?;
    let mut output = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    mp4::write(&mut output, &clip.stream, &clip.packets)?;
    let file = output.into_inner().map_err(|error| SaveError::Io {
        step: "finish",
        path: path.to_owned(),
        source: error.into_error(),
    })?;
    file.sync_all().map_err(|source| SaveError::Io {
        step: "finish",
        path: path.to_owned(),
        source,
    })
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
        assert!(is_keyframe(Codec::H264, &keyframe()));
        assert!(!is_keyframe(Codec::H264, &inter_frame()));
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

            let mut ring = ring::Ring::new(40);
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
            };
            assert_eq!(clip.packets.len(), 45, "{codec}");

            let mut file = Vec::new();
            mp4::write(&mut file, &clip.stream, &clip.packets).unwrap();
            let top = mp4::tests::atoms(&file);
            assert_eq!(top.len(), 3, "{codec}");
            if let Some(directory) = std::env::var_os("HINDSIGHT_MUX_OUTPUT") {
                let name = format!("{}.mp4", codec.name().replace('.', "").to_lowercase());
                fs::write(Path::new(&directory).join(name), &file).unwrap();
            }
        }
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
        };

        assert_matches!(
            save(&clip, directory.path()),
            Err(SaveError::Mux(MuxError::NoSequenceHeader))
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
