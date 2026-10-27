use crate::{
    config::{Backend, ScreenshotFormat},
    saving::{self, WriteError},
    service::MonitorHandle,
};
use std::{
    error::Error,
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;

const BYTES_PER_PIXEL: usize = 4;
const JPEG_QUALITY: u8 = 92;
const AVIF_QUALITY: f32 = 80.0;
/// The fastest of ravif's speeds: about two seconds of one core for a 1440p picture.
const AVIF_SPEED: u8 = 10;

/// A picture as the encoder receives it: 8-bit BGRA rows, top to bottom, with no padding.
pub(crate) struct Still {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

impl Still {
    /// Copies `height` rows of `width` pixels that start `pitch` bytes apart in `bytes`. `None`
    /// when the rows do not fit in `bytes` or overlap.
    pub(crate) fn from_pitched(
        width: u32,
        height: u32,
        pitch: usize,
        bytes: &[u8],
    ) -> Option<Self> {
        let span = pitched_span(width, height, pitch)?;
        let row = usize::try_from(width).ok()?.checked_mul(BYTES_PER_PIXEL)?;
        let mut bgra = Vec::with_capacity(row.checked_mul(usize::try_from(height).ok()?)?);
        for line in bytes.get(..span)?.chunks(pitch) {
            bgra.extend_from_slice(line.get(..row)?);
        }
        Some(Self {
            width,
            height,
            bgra,
        })
    }

    /// The pixels as RGB. Alpha is dropped: captured pictures are opaque, and some backends
    /// leave it zero.
    fn rgb(&self) -> Vec<u8> {
        let (pixels, _) = self.bgra.as_chunks::<BYTES_PER_PIXEL>();
        pixels
            .iter()
            .flat_map(|[blue, green, red, _]| [*red, *green, *blue])
            .collect()
    }
}

/// The bytes from the start of the first of `height` rows of `width` pixels, `pitch` bytes
/// apart, to the end of the last. `None` for no pixels or rows that overlap.
pub(crate) fn pitched_span(width: u32, height: u32, pitch: usize) -> Option<usize> {
    let row = usize::try_from(width).ok()?.checked_mul(BYTES_PER_PIXEL)?;
    let later_rows = usize::try_from(height).ok()?.checked_sub(1)?;
    if row == 0 || pitch < row {
        return None;
    }
    pitch.checked_mul(later_rows)?.checked_add(row)
}

impl fmt::Debug for Still {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Still")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// A picture the capture thread has copied on the GPU. `finish` waits for the copy and reads it
/// into memory on the thread that saves it, so the capture thread never waits.
pub(crate) trait Readback: Send + fmt::Debug {
    fn finish(self: Box<Self>) -> Result<Still, StillError>;
}

impl Readback for Still {
    fn finish(self: Box<Self>) -> Result<Still, StillError> {
        Ok(*self)
    }
}

/// What the capture thread answers a screenshot request with.
pub(crate) type Taken = Result<Box<dyn Readback>, StillError>;

/// A capture of one picture of a whole monitor, for a screenshot that no recording can serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grab {
    /// `None` is the primary monitor.
    pub(crate) monitor: Option<MonitorHandle>,
    /// The backend chosen for recording, which the grab prefers where it can.
    pub(crate) backend: Backend,
    pub(crate) cursor: bool,
}

/// Takes a [`Grab`] on the calling thread.
pub(crate) type Grabber = fn(&Grab) -> Result<Still, StillError>;

type Cause = Box<dyn Error + Send + Sync>;

#[derive(Debug, Error)]
pub(crate) enum StillError {
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture reports it")
    )]
    #[error("capture has not received a picture yet")]
    NoPicture,
    #[error("capture stopped before taking the picture")]
    Abandoned,
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture reports it")
    )]
    #[error("copy the picture on the GPU")]
    Copy(#[source] Cause),
    #[error("capture the monitor")]
    Grab(#[source] Cause),
    #[error("read the picture back from the GPU")]
    Read(#[source] Cause),
    #[error("the picture read back is smaller than {width}×{height}")]
    Truncated { width: u32, height: u32 },
}

/// The answer to one screenshot request. Dropped unanswered, it answers
/// [`StillError::Abandoned`], so whoever asked always hears back once.
pub(crate) struct Reply(Option<Box<dyn FnOnce(Taken) + Send>>);

impl Reply {
    pub(crate) fn new(answer: impl FnOnce(Taken) + Send + 'static) -> Self {
        Self(Some(Box::new(answer)))
    }

    pub(crate) fn send(mut self, taken: Taken) {
        if let Some(answer) = self.0.take() {
            answer(taken);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(answer) = self.0.take() {
            answer(Err(StillError::Abandoned));
        }
    }
}

/// At most one screenshot request waiting for the capture thread, which looks for one each
/// frame with a single atomic load.
#[derive(Default)]
pub(crate) struct Requests {
    /// Written only while `pending` is locked.
    wanted: AtomicBool,
    pending: Mutex<Option<Reply>>,
}

impl Requests {
    /// A request still waiting is replaced, and answered as abandoned.
    pub(crate) fn ask(&self, reply: Reply) {
        let replaced = {
            let mut pending = self.pending();
            let replaced = pending.replace(reply);
            self.wanted.store(true, Ordering::Release);
            replaced
        };
        drop(replaced);
    }

    pub(crate) fn take(&self) -> Option<Reply> {
        if !self.wanted.load(Ordering::Acquire) {
            return None;
        }
        self.withdraw()
    }

    /// Removes the waiting request, for when nothing will take it.
    pub(crate) fn withdraw(&self) -> Option<Reply> {
        let mut pending = self.pending();
        self.wanted.store(false, Ordering::Relaxed);
        pending.take()
    }

    fn pending(&self) -> MutexGuard<'_, Option<Reply>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ScreenshotError {
    #[error(transparent)]
    Picture(#[from] StillError),
    #[error(transparent)]
    Save(#[from] WriteError),
    #[error("write {}", path.display())]
    Encode {
        path: PathBuf,
        #[source]
        source: EncodeError,
    },
}

#[derive(Debug, Error)]
pub(crate) enum EncodeError {
    #[error(transparent)]
    Png(#[from] png::EncodingError),
    #[error(transparent)]
    Jpeg(#[from] jpeg_encoder::EncodingError),
    #[error(transparent)]
    Webp(#[from] image_webp::EncodingError),
    #[error(transparent)]
    Avif(#[from] ravif::Error),
    #[error(transparent)]
    Write(#[from] std::io::Error),
    #[error("a JPEG holds at most 65535×65535 pixels, not {width}×{height}")]
    TooLargeForJpeg { width: u32, height: u32 },
}

/// Reads the picture back and writes it to `directory` as `hindsight_<date>_<time>` in
/// `format`.
pub(crate) fn save(
    readback: Box<dyn Readback>,
    directory: &Path,
    format: ScreenshotFormat,
) -> Result<PathBuf, ScreenshotError> {
    let still = readback.finish()?;
    let (path, ()) = saving::save_file(directory, format.extension(), |partial| {
        saving::write_file(partial, |output| {
            encode(&still, output, format).map_err(|source| ScreenshotError::Encode {
                path: partial.to_owned(),
                source,
            })
        })
    })?;
    Ok(path)
}

/// The picture is sRGB: an HDR display's is tone-mapped to it before the encoder sees it. PNG
/// and WebP are lossless.
fn encode(
    still: &Still,
    mut output: impl Write,
    format: ScreenshotFormat,
) -> Result<(), EncodeError> {
    let rgb = still.rgb();
    match format {
        ScreenshotFormat::Png => {
            let mut encoder = png::Encoder::new(output, still.width, still.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&rgb)?;
            writer.finish()?;
        }
        ScreenshotFormat::Jpeg => {
            let too_large = || EncodeError::TooLargeForJpeg {
                width: still.width,
                height: still.height,
            };
            let width = u16::try_from(still.width).map_err(|_| too_large())?;
            let height = u16::try_from(still.height).map_err(|_| too_large())?;
            jpeg_encoder::Encoder::new(&mut output, JPEG_QUALITY).encode(
                &rgb,
                width,
                height,
                jpeg_encoder::ColorType::Rgb,
            )?;
        }
        ScreenshotFormat::Webp => image_webp::WebPEncoder::new(&mut output).encode(
            &rgb,
            still.width,
            still.height,
            image_webp::ColorType::Rgb8,
        )?,
        ScreenshotFormat::Avif => {
            let (pixels, _) = rgb.as_chunks::<3>();
            let pixels: Vec<ravif::RGB8> = pixels
                .iter()
                .map(|&[red, green, blue]| ravif::RGB8::new(red, green, blue))
                .collect();
            let image = ravif::Img::new(&pixels[..], still.width as usize, still.height as usize);
            let encoded = ravif::Encoder::new()
                .with_quality(AVIF_QUALITY)
                .with_speed(AVIF_SPEED)
                .encode_rgb(image)?;
            output.write_all(&encoded.avif_file)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        sync::{Arc, mpsc},
    };

    /// Two pixels across and two down, in rows padded to 12 bytes.
    const PITCHED: [u8; 20] = [
        1, 2, 3, 255, 4, 5, 6, 255, 0xEE, 0xEE, 0xEE, 0xEE, //
        7, 8, 9, 255, 10, 11, 12, 0,
    ];

    #[test]
    fn rows_are_copied_without_their_padding() {
        let still = Still::from_pitched(2, 2, 12, &PITCHED).unwrap();
        assert_eq!(
            still.bgra,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 0]
        );
        assert_eq!(still.rgb(), [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
        assert_eq!(
            Still::from_pitched(2, 2, 12, &PITCHED[..19]).map(|_| ()),
            None
        );
        assert_eq!(Still::from_pitched(2, 2, 7, &PITCHED).map(|_| ()), None);
        assert_eq!(Still::from_pitched(0, 2, 12, &PITCHED).map(|_| ()), None);
        assert_eq!(Still::from_pitched(2, 0, 12, &PITCHED).map(|_| ()), None);
        assert_eq!(
            Still::from_pitched(u32::MAX, u32::MAX, usize::MAX, &PITCHED).map(|_| ()),
            None
        );
    }

    #[test]
    fn a_screenshot_is_saved_as_a_png_named_for_the_time() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("screenshots");
        let still = Still::from_pitched(2, 2, 12, &PITCHED).unwrap();

        let first = save(Box::new(still), &folder, ScreenshotFormat::Png).unwrap();
        let still = Still::from_pitched(2, 2, 12, &PITCHED).unwrap();
        let second = save(Box::new(still), &folder, ScreenshotFormat::Png).unwrap();

        assert_ne!(first, second);
        assert!(first.starts_with(&folder));
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("hindsight_"), "{name}");
        assert_eq!(first.extension().unwrap(), "png");
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 2);

        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&first).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.color_type, png::ColorType::Rgb);
        assert_eq!(pixels, [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn jpeg_webp_and_avif_screenshots_carry_their_own_signatures() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("screenshots");
        for (format, extension, signature) in [
            (ScreenshotFormat::Jpeg, "jpg", &b"\xFF\xD8\xFF"[..]),
            (ScreenshotFormat::Webp, "webp", &b"RIFF"[..]),
            (ScreenshotFormat::Avif, "avif", &b"\0\0\0"[..]),
        ] {
            let still = Still::from_pitched(2, 2, 12, &PITCHED).unwrap();
            let path = save(Box::new(still), &folder, format).unwrap();
            assert_eq!(path.extension().unwrap(), extension);
            let bytes = fs::read(&path).unwrap();
            assert!(bytes.starts_with(signature), "{format}");
        }
        let webp = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "webp")
            })
            .unwrap();
        assert_eq!(&fs::read(webp).unwrap()[8..12], b"WEBP");
        let avif = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "avif")
            })
            .unwrap();
        assert_eq!(&fs::read(avif).unwrap()[4..12], b"ftypavif");
    }

    #[test]
    fn a_failed_readback_writes_nothing() {
        #[derive(Debug)]
        struct Lost;
        impl Readback for Lost {
            fn finish(self: Box<Self>) -> Result<Still, StillError> {
                Err(StillError::Truncated {
                    width: 2,
                    height: 2,
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("screenshots");

        let error = save(Box::new(Lost), &folder, ScreenshotFormat::Png).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the picture read back is smaller than 2×2"
        );
        assert!(!folder.exists());
    }

    fn answers() -> (Reply, mpsc::Receiver<Taken>) {
        let (sender, receiver) = mpsc::channel();
        let reply = Reply::new(move |taken| sender.send(taken).unwrap());
        (reply, receiver)
    }

    fn still() -> Box<dyn Readback> {
        Box::new(Still::from_pitched(1, 1, 4, &[0; 4]).unwrap())
    }

    #[test]
    fn a_request_is_taken_once_and_answered_once() {
        let requests = Requests::default();
        assert!(requests.take().is_none());

        let (reply, answers) = answers();
        requests.ask(reply);
        let taken = requests.take().unwrap();
        assert!(requests.take().is_none());
        taken.send(Ok(still()));

        assert!(answers.recv().unwrap().is_ok());
        assert!(answers.try_recv().is_err(), "a reply answers once");
    }

    #[test]
    fn a_request_nobody_takes_is_answered_as_abandoned() {
        let requests = Requests::default();
        let (first, first_answers) = answers();
        let (second, second_answers) = answers();

        requests.ask(first);
        requests.ask(second);
        assert!(matches!(
            first_answers.recv().unwrap(),
            Err(StillError::Abandoned)
        ));

        drop(requests.withdraw());
        assert!(matches!(
            second_answers.recv().unwrap(),
            Err(StillError::Abandoned)
        ));
        assert!(requests.take().is_none());

        let requests = Arc::new(Requests::default());
        let (third, third_answers) = answers();
        requests.ask(third);
        drop(requests);
        assert!(matches!(
            third_answers.recv().unwrap(),
            Err(StillError::Abandoned)
        ));
    }

    #[test]
    fn the_cause_is_printed_once_through_the_chain() {
        let error = ScreenshotError::from(StillError::Read(Box::new(std::io::Error::other(
            "device removed",
        ))));
        assert_eq!(
            crate::log::chain(&error).to_string(),
            "read the picture back from the GPU: device removed"
        );
    }
}
