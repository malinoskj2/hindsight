//! The AAC-LC encoder: Media Foundation's AAC encoder transform, which ships with Windows (it is
//! what NVIDIA Instant Replay's clips carry too). It takes 16-bit PCM and returns one raw AAC
//! frame of 1024 samples per output, about 47 a second at 48 kHz. Encoding a frame takes tens
//! of microseconds, so it runs on the audio capture thread, straight after each packet.

use crate::replay::{CLOCK_RATE, FRAME_SAMPLES};
use std::{mem::ManuallyDrop, sync::Arc};
use windows::{
    Win32::{
        Media::MediaFoundation::{
            AACMFTEncoder, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
            MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
            MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
            MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA, MF_VERSION, MFAudioFormat_AAC,
            MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
            MFMediaType_Audio, MFSTARTUP_LITE, MFShutdown, MFStartup,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
        },
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    },
    core::Result,
};

/// The bitrate, 160 kb/s, in the bytes a second the encoder is configured with; it accepts
/// 12 000, 16 000, 20 000 and 24 000.
const BYTES_PER_SECOND: u32 = 20_000;
/// AAC-LC at level 2, as the encoder documents it for stereo up to 48 kHz.
const AAC_LC_LEVEL_2: u32 = 0x29;
/// Raw AAC frames, without ADTS headers: the MP4 file describes the stream once.
const RAW_PAYLOAD: u32 = 0;
/// The bits of a PCM sample the encoder takes.
const PCM_BITS: u32 = 16;
/// The `HEAACWAVEINFO` fields in front of the `AudioSpecificConfig` in the output type's user
/// data.
const WAVE_INFO_BYTES: usize = 12;
/// The samples the encoder puts in front of its input: decoding its frames from the first
/// yields this many samples before the first one it was given. Microsoft's encoder puts none:
/// it holds back almost three frames of input (2944 samples after a second of it) and uses
/// them to make its first frame decode to the first input sample. Measured by encoding a click
/// and finding it in ffmpeg's decoding of the frames (the `encoder_delay` test, which needs
/// ffmpeg and is run by hand).
pub(crate) const PRIMING: u32 = 0;
/// The input the encoder can hold back before it finishes a frame, in samples per channel: its
/// lookahead and a partly filled frame, rounded up.
pub(crate) const HELD_BACK: u32 = 4 * FRAME_SAMPLES;
/// Samples of silence encoded to find the frame the encoder makes of silence.
const SILENCE_PROBE_FRAMES: usize = 8;

/// Media Foundation, started for the calling thread's use; stopped again on drop.
pub(crate) struct MediaFoundation(());

impl MediaFoundation {
    /// Starts Media Foundation without its sockets layer, which an encoder does not need. COM
    /// must be initialised on the thread.
    pub(crate) fn start() -> Result<Self> {
        // SAFETY: a plain call; every successful start is matched by the shutdown in `drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }?;
        Ok(Self(()))
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: matches the successful start that made `self`.
        let _ = unsafe { MFShutdown() };
    }
}

/// An open encoder for 16-bit interleaved PCM at one rate and channel count.
pub(crate) struct Encoder {
    transform: IMFTransform,
    rate: u32,
    channels: u16,
    /// The `AudioSpecificConfig` of the stream it produces.
    config: Box<[u8]>,
    /// The sample the encoder writes each frame into, when it does not bring its own.
    output: Option<IMFSample>,
    /// Samples per channel handed to the encoder so far, for the input timestamps.
    fed: u64,
}

impl Encoder {
    /// Opens the encoder for `channels` channels at `rate` samples a second.
    pub(crate) fn open(rate: u32, channels: u16) -> Result<Self> {
        // SAFETY: creates the in-process encoder; COM is initialised on this thread.
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER) }?;
        set_types(&transform, rate, channels)?;
        let config = specific_config(&transform)?;
        let output = start_streaming(&transform)?;
        Ok(Self {
            transform,
            rate,
            channels,
            config,
            output,
            fed: 0,
        })
    }

    /// The `AudioSpecificConfig` of the stream, for the MP4 file's `esds` box.
    pub(crate) fn config(&self) -> Box<[u8]> {
        self.config.clone()
    }

    /// Encodes `pcm`, interleaved samples of every channel, and hands each frame the encoder
    /// finishes to `frame`, in order.
    pub(crate) fn encode(&mut self, pcm: &[i16], mut frame: impl FnMut(Arc<[u8]>)) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = (pcm.len() / usize::from(self.channels.max(1))) as u64;
        let input = self.input_sample(pcm, frames)?;
        self.fed += frames;
        // SAFETY: the encoder takes its own reference to the sample.
        unsafe { self.transform.ProcessInput(0, &input, 0) }?;
        while let Some(data) = self.next_frame()? {
            frame(data);
        }
        Ok(())
    }

    /// A Media Foundation sample holding `pcm` as little-endian 16-bit samples (what Windows
    /// runs on), the `frames` samples per channel after the ones fed so far.
    fn input_sample(&self, pcm: &[i16], frames: u64) -> Result<IMFSample> {
        let length = u32::try_from(size_of_val(pcm)).unwrap_or(u32::MAX);
        let time = |samples: u64| {
            i64::try_from(i128::from(samples) * i128::from(CLOCK_RATE) / i128::from(self.rate))
                .unwrap_or(i64::MAX)
        };
        // SAFETY: the buffer is created with room for `length` bytes, locked while the samples
        // are copied into it byte for byte, and unlocked before it is handed to the sample.
        unsafe {
            let buffer = MFCreateMemoryBuffer(length)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&raw mut data, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), data, length as usize);
            buffer.Unlock()?;
            buffer.SetCurrentLength(length)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time(self.fed))?;
            sample.SetSampleDuration(time(self.fed + frames) - time(self.fed))?;
            Ok(sample)
        }
    }

    /// The next finished frame, or `None` when the encoder needs more input.
    fn next_frame(&self) -> Result<Option<Arc<[u8]>>> {
        if let Some(sample) = &self.output {
            // SAFETY: the sample made in `open` holds one buffer.
            unsafe { sample.GetBufferByIndex(0)?.SetCurrentLength(0) }?;
        }
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(self.output.clone()),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        // SAFETY: one output buffer for the encoder's one stream; the sample in it, if any, is
        // ours and has room for a frame.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, &mut buffers, &raw mut status)
        };
        // Take back the references the call left in the buffer, so they are released.
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(error) => Err(error),
            Ok(()) => sample.map(|sample| contents(&sample)).transpose(),
        }
    }
}

/// The most bytes one AAC-LC frame can take: 6144 bits per channel, for two channels.
const FRAME_BYTES_MAX: u32 = 2 * 6144 / 8;

/// Gives `transform` its output type, AAC at the bitrate, and then its input type, 16-bit PCM
/// of `channels` channels at `rate`; the encoder wants them in that order.
fn set_types(transform: &IMFTransform, rate: u32, channels: u16) -> Result<()> {
    let output_type = audio_type(&MFAudioFormat_AAC, rate, channels)?;
    // SAFETY: attributes of a live media type; then the type is handed to the encoder.
    unsafe {
        output_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, BYTES_PER_SECOND)?;
        output_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, RAW_PAYLOAD)?;
        output_type.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL_2)?;
        transform.SetOutputType(0, &output_type, 0)?;
    }
    let input_type = audio_type(&MFAudioFormat_PCM, rate, channels)?;
    let block = u32::from(channels) * PCM_BITS / 8;
    // SAFETY: as above.
    unsafe {
        input_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
        input_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, block * rate)?;
        transform.SetInputType(0, &input_type, 0)?;
    }
    Ok(())
}

/// Tells the configured `transform` that streaming starts, and makes the sample its frames are
/// written into, unless it brings its own.
fn start_streaming(transform: &IMFTransform) -> Result<Option<IMFSample>> {
    // SAFETY: queries and messages to the configured encoder.
    unsafe {
        let info = transform.GetOutputStreamInfo(0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0 {
            return Ok(None);
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(FRAME_BYTES_MAX))?)?;
        Ok(Some(sample))
    }
}

/// A media type for `subtype` audio of 16-bit samples, `channels` channels at `rate`.
fn audio_type(subtype: &windows::core::GUID, rate: u32, channels: u16) -> Result<IMFMediaType> {
    // SAFETY: a new media type, and attributes set on it.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, PCM_BITS)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(channels))?;
        Ok(media_type)
    }
}

/// The `AudioSpecificConfig` the encoder reports in its output type, behind the `HEAACWAVEINFO`
/// fields.
fn specific_config(transform: &IMFTransform) -> Result<Box<[u8]>> {
    // SAFETY: reads a blob of the reported size from the encoder's current output type.
    let user_data = unsafe {
        let output_type = transform.GetOutputCurrentType(0)?;
        let size = output_type.GetBlobSize(&MF_MT_USER_DATA)?;
        let mut user_data = vec![0; size as usize];
        output_type.GetBlob(&MF_MT_USER_DATA, &mut user_data, None)?;
        user_data
    };
    Ok(user_data.get(WAVE_INFO_BYTES..).unwrap_or_default().into())
}

/// The bytes of a finished output sample.
fn contents(sample: &IMFSample) -> Result<Arc<[u8]>> {
    // SAFETY: the buffer is locked while its current length is copied out.
    unsafe {
        let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&raw mut data, None, Some(&raw mut length))?;
        let frame = Arc::from(std::slice::from_raw_parts(data, length as usize));
        buffer.Unlock()?;
        Ok(frame)
    }
}

/// The frame the encoder makes of digital silence once it has settled, from a throwaway
/// encoder so the session's stays untouched.
pub(crate) fn silent_frame(rate: u32, channels: u16) -> Result<Arc<[u8]>> {
    let mut encoder = Encoder::open(rate, channels)?;
    let zeros = vec![0; SILENCE_PROBE_FRAMES * FRAME_SAMPLES as usize * usize::from(channels)];
    let mut last = None;
    encoder.encode(&zeros, |frame| last = Some(frame))?;
    last.ok_or_else(|| windows::core::Error::from(MF_E_TRANSFORM_NEED_MORE_INPUT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::loopback::Com;

    /// Opens an encoder, or says why a machine without Media Foundation (a Windows N edition
    /// without the Media Feature Pack, a server without the feature) skips the test.
    fn encoder() -> Option<(Com, MediaFoundation, Encoder)> {
        let com = Com::start().unwrap();
        let media = MediaFoundation::start().ok()?;
        match Encoder::open(48_000, 2) {
            Ok(encoder) => Some((com, media, encoder)),
            Err(error) => {
                eprintln!("skipped: no AAC encoder ({error})");
                None
            }
        }
    }

    #[test]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the tone stays within ±8000"
    )]
    fn encodes_a_frame_per_1024_samples_and_reports_its_configuration() {
        let Some((_com, _media, mut encoder)) = encoder() else {
            return;
        };
        // AAC-LC, 48 kHz (index 3), two channels.
        assert_eq!(&*encoder.config(), &[0x11, 0x90]);

        // A 440 Hz tone on both channels for a second.
        let tone: Vec<i16> = (0..48_000 * 2)
            .map(|index| (f64::from(index / 2) * 0.0576).sin() * 8_000.0)
            .map(|sample| sample as i16)
            .collect();
        let mut frames = Vec::new();
        for chunk in tone.chunks(960) {
            encoder.encode(chunk, |frame| frames.push(frame)).unwrap();
        }
        // One second is 46.9 frames; the encoder holds back what its delay needs.
        assert!((44..=47).contains(&frames.len()), "{}", frames.len());
        let bytes: usize = frames.iter().map(|frame| frame.len()).sum();
        assert!(
            (15_000..=25_000).contains(&bytes),
            "{bytes} bytes for a second"
        );

        let silence = silent_frame(48_000, 2).unwrap();
        // The encoder keeps a constant bitrate, so silence takes a full frame's worth of bytes.
        assert!((300..=FRAME_BYTES_MAX as usize).contains(&silence.len()));
    }

    /// Finds the encoder's delay: encodes a click at sample 10 000, writes the frames as ADTS
    /// and prints where ffmpeg's decoder puts it. Run by hand with ffmpeg on the path:
    /// `cargo test encoder_delay -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs ffmpeg"]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "ADTS length fields and sample positions are small"
    )]
    fn encoder_delay() {
        let (_com, _media, mut encoder) = encoder().unwrap();
        let mut pcm = vec![0_i16; 48_000 * 2];
        pcm[20_000] = 30_000;
        pcm[20_001] = 30_000;
        let mut adts = Vec::new();
        for chunk in pcm.chunks(960) {
            encoder
                .encode(chunk, |frame| {
                    let length = frame.len() + 7;
                    adts.extend([
                        0xFF,
                        0xF1,
                        // AAC-LC, 48 kHz, two channels, then the 13-bit length.
                        0x4C,
                        0x80 | ((length >> 11) & 0x03) as u8,
                        ((length >> 3) & 0xFF) as u8,
                        (((length & 0x07) << 5) | 0x1F) as u8,
                        0xFC,
                    ]);
                    adts.extend_from_slice(&frame);
                })
                .unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("click.aac");
        let output = directory.path().join("click.raw");
        std::fs::write(&input, &adts).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&input)
            .args(["-f", "s16le", "-ac", "1"])
            .arg(&output)
            .status()
            .unwrap();
        assert!(status.success());
        let decoded: Vec<i16> = std::fs::read(&output)
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let peak = (0..decoded.len())
            .max_by_key(|&index| decoded[index].unsigned_abs())
            .unwrap();
        println!(
            "click at 10000, decoded peak at {peak}: delay {}; {} bytes of ADTS, {} samples decoded",
            peak as i64 - 10_000,
            adts.len(),
            decoded.len()
        );
        assert_eq!(peak as i64 - 10_000, i64::from(PRIMING));
    }
}
