//! AAC-LC encoding with Media Foundation's AAC MFT, run on the save thread over a clip's PCM.

use super::EncodeError;
use crate::{
    replay::{AudioClip, FRAME_SAMPLES, PcmClip, ticks_of},
    sys::com::Apartment,
};
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
    core::{GUID, Result},
};

/// 192 kb/s; the encoder accepts 12 000, 16 000, 20 000 and 24 000 bytes a second.
const BYTES_PER_SECOND: u32 = 24_000;
const AAC_LC_LEVEL_2: u32 = 0x29;
const RAW_PAYLOAD: u32 = 0;
const PCM_BITS: u32 = 16;
/// `HEAACWAVEINFO` precedes the `AudioSpecificConfig` in the output type's user data.
const WAVE_INFO_BYTES: usize = 12;
/// Samples the encoder prepends to its input; Microsoft's prepends none (see `encoder_delay`).
pub(super) const PRIMING: u32 = 0;
/// Most input the encoder holds back before finishing a frame: lookahead plus a partial frame.
const HELD_BACK: u32 = 4 * FRAME_SAMPLES;
/// 6144 bits per channel, two channels.
const FRAME_BYTES_MAX: u32 = 2 * 6144 / 8;
/// Samples fed to the encoder at a time.
const CLIP_BLOCK: u32 = 8 * FRAME_SAMPLES;

struct MediaFoundation(());

impl MediaFoundation {
    /// COM must be initialised on the calling thread.
    fn start() -> Result<Self> {
        // SAFETY: paired with the shutdown in `drop`.
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

struct Encoder {
    transform: IMFTransform,
    rate: u32,
    channels: u16,
    config: Box<[u8]>,
    /// Reused output sample, unless the encoder provides its own.
    output: Option<IMFSample>,
    fed: u64,
}

impl Encoder {
    fn open(rate: u32, channels: u16) -> Result<Self> {
        // SAFETY: COM is initialised on this thread.
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
    fn config(&self) -> Box<[u8]> {
        self.config.clone()
    }

    fn encode(&mut self, pcm: &[i16], mut frame: impl FnMut(Arc<[u8]>)) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = (pcm.len() / usize::from(self.channels)) as u64;
        let input = self.input_sample(pcm, frames)?;
        self.fed += frames;
        // SAFETY: the encoder takes its own reference to the sample.
        unsafe { self.transform.ProcessInput(0, &input, 0) }?;
        while let Some(data) = self.next_frame()? {
            frame(data);
        }
        Ok(())
    }

    fn input_sample(&self, pcm: &[i16], frames: u64) -> Result<IMFSample> {
        let length = u32::try_from(size_of_val(pcm)).unwrap_or(u32::MAX);
        let time = |samples: u64| ticks_of(samples, self.rate);
        // SAFETY: takes a plain size and reads no caller memory.
        let buffer = unsafe { MFCreateMemoryBuffer(length) }?;
        let mut data = std::ptr::null_mut();
        let mut capacity = 0;
        // SAFETY: the out-pointers are to locals that outlive the call.
        unsafe { buffer.Lock(&raw mut data, Some(&raw mut capacity), None) }?;
        assert!(
            capacity >= length,
            "a memory buffer holds less than was asked for"
        );
        // SAFETY: the lock makes `data` writable for `capacity >= length` bytes until the unlock
        // below; `pcm` is `length` bytes or more and is not memory the new buffer owns.
        unsafe { std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), data, length as usize) };
        // SAFETY: releases the lock taken above; `data` is not used again.
        unsafe { buffer.Unlock() }?;
        // SAFETY: `length` bytes were written above, within the buffer's capacity.
        unsafe { buffer.SetCurrentLength(length) }?;
        // SAFETY: takes no arguments.
        let sample = unsafe { MFCreateSample() }?;
        // SAFETY: the sample takes its own reference to the unlocked buffer.
        unsafe { sample.AddBuffer(&buffer) }?;
        // SAFETY: a plain value set on the sample `sample` owns a reference to.
        unsafe { sample.SetSampleTime(time(self.fed)) }?;
        // SAFETY: as above.
        unsafe { sample.SetSampleDuration(time(self.fed + frames) - time(self.fed)) }?;
        Ok(sample)
    }

    fn next_frame(&self) -> Result<Option<Arc<[u8]>>> {
        if let Some(sample) = &self.output {
            // SAFETY: an index the sample made in `open` holds: it has one buffer.
            let buffer = unsafe { sample.GetBufferByIndex(0) }?;
            // SAFETY: an empty length is within any buffer.
            unsafe { buffer.SetCurrentLength(0) }?;
        }
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(self.output.clone()),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        // SAFETY: one output buffer for the one stream; its sample, if any, has room for a frame.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, &mut buffers, &raw mut status)
        };
        // Reclaim the references ProcessOutput left in the buffer.
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

/// The encoder wants its output type set before its input type.
fn set_types(transform: &IMFTransform, rate: u32, channels: u16) -> Result<()> {
    let output_type = audio_type(&MFAudioFormat_AAC, rate, channels)?;
    set_u32(
        &output_type,
        &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        BYTES_PER_SECOND,
    )?;
    set_u32(&output_type, &MF_MT_AAC_PAYLOAD_TYPE, RAW_PAYLOAD)?;
    set_u32(
        &output_type,
        &MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
        AAC_LC_LEVEL_2,
    )?;
    // SAFETY: the encoder takes its own reference to the type, borrowed for the call.
    unsafe { transform.SetOutputType(0, &output_type, 0) }?;
    let input_type = audio_type(&MFAudioFormat_PCM, rate, channels)?;
    let block = u32::from(channels) * PCM_BITS / 8;
    set_u32(&input_type, &MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
    set_u32(&input_type, &MF_MT_AUDIO_AVG_BYTES_PER_SECOND, block * rate)?;
    // SAFETY: as above.
    unsafe { transform.SetInputType(0, &input_type, 0) }?;
    Ok(())
}

fn set_u32(media_type: &IMFMediaType, key: &GUID, value: u32) -> Result<()> {
    // SAFETY: `key` is borrowed for the call and the value is copied.
    unsafe { media_type.SetUINT32(key, value) }
}

fn set_guid(media_type: &IMFMediaType, key: &GUID, value: &GUID) -> Result<()> {
    // SAFETY: `key` and `value` are borrowed for the call and copied.
    unsafe { media_type.SetGUID(key, value) }
}

fn start_streaming(transform: &IMFTransform) -> Result<Option<IMFSample>> {
    // SAFETY: stream 0 is the encoder's only output stream; the info is returned by value.
    let info = unsafe { transform.GetOutputStreamInfo(0) }?;
    // SAFETY: a message with no data, sent once the types are set.
    unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }?;
    // SAFETY: as above.
    unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }?;
    if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0 {
        return Ok(None);
    }
    // SAFETY: takes no arguments.
    let sample = unsafe { MFCreateSample() }?;
    // SAFETY: takes a plain size and reads no caller memory.
    let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(FRAME_BYTES_MAX)) }?;
    // SAFETY: the sample takes its own reference to the buffer.
    unsafe { sample.AddBuffer(&buffer) }?;
    Ok(Some(sample))
}

fn audio_type(subtype: &GUID, rate: u32, channels: u16) -> Result<IMFMediaType> {
    // SAFETY: takes no arguments.
    let media_type = unsafe { MFCreateMediaType() }?;
    set_guid(&media_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    set_guid(&media_type, &MF_MT_SUBTYPE, subtype)?;
    set_u32(&media_type, &MF_MT_AUDIO_BITS_PER_SAMPLE, PCM_BITS)?;
    set_u32(&media_type, &MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
    set_u32(&media_type, &MF_MT_AUDIO_NUM_CHANNELS, u32::from(channels))?;
    Ok(media_type)
}

fn specific_config(transform: &IMFTransform) -> Result<Box<[u8]>> {
    // SAFETY: stream 0 is the encoder's only output stream, whose type `set_types` set.
    let output_type = unsafe { transform.GetOutputCurrentType(0) }?;
    // SAFETY: `MF_MT_USER_DATA` is borrowed for the call.
    let size = unsafe { output_type.GetBlobSize(&MF_MT_USER_DATA) }?;
    let mut user_data = vec![0; size as usize];
    // SAFETY: the blob is written into `user_data`, whose length the call is given.
    unsafe { output_type.GetBlob(&MF_MT_USER_DATA, &mut user_data, None) }?;
    Ok(user_data.get(WAVE_INFO_BYTES..).unwrap_or_default().into())
}

fn contents(sample: &IMFSample) -> Result<Arc<[u8]>> {
    // SAFETY: the sample is live; the call returns a new reference to one buffer.
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }?;
    let mut data = std::ptr::null_mut();
    let mut length = 0;
    // SAFETY: the out-pointers are to locals that outlive the call.
    unsafe { buffer.Lock(&raw mut data, None, Some(&raw mut length)) }?;
    // SAFETY: the lock makes `data` readable for the buffer's current `length` bytes until the
    // unlock below, and the slice is copied into the `Arc` before then.
    let frame = Arc::from(unsafe { std::slice::from_raw_parts(data, length as usize) });
    // SAFETY: releases the lock taken above; `data` is not used again.
    unsafe { buffer.Unlock() }?;
    Ok(frame)
}

fn failed(step: &'static str) -> impl FnOnce(windows::core::Error) -> EncodeError {
    move |error| EncodeError::Windows(step, error)
}

/// A fresh encoder started at the clip's first sample keeps its frames in step with the video;
/// the input runs `HELD_BACK` past the clip's end so the encoder finishes the last frame.
pub(super) fn encode_clip(clip: &PcmClip) -> std::result::Result<AudioClip, EncodeError> {
    let _com = Apartment::multi_threaded().map_err(failed("initialise COM"))?;
    let _media = MediaFoundation::start().map_err(failed("start Media Foundation"))?;
    let format = &clip.format;
    let mut encoder = Encoder::open(format.sample_rate.get(), format.channels.get())
        .map_err(failed("open the AAC encoder"))?;
    let mut frames = Vec::with_capacity(clip.frames);
    let mut pcm = vec![0; CLIP_BLOCK as usize * usize::from(format.channels.get())];
    let input = i64::try_from(clip.frames)
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(FRAME_SAMPLES))
        .saturating_add(i64::from(HELD_BACK));
    let end = clip.start.saturating_add(input);
    let mut next = clip.start;
    while frames.len() < clip.frames && next < end {
        clip.read(next, &mut pcm);
        encoder
            .encode(&pcm, |frame| frames.push(frame))
            .map_err(failed("encode the sound"))?;
        next += i64::from(CLIP_BLOCK);
    }
    if frames.len() < clip.frames {
        return Err(EncodeError::Short {
            made: frames.len(),
            needed: clip.frames,
        });
    }
    frames.truncate(clip.frames);
    Ok(AudioClip {
        sample_rate: format.sample_rate.get(),
        channels: format.channels.get(),
        config: encoder.config(),
        frames,
        lead: clip.lead,
    })
}

#[cfg(test)]
mod tests {
    use super::{super::testing, *};

    fn encoder() -> Option<(Apartment, MediaFoundation, Encoder)> {
        let com = Apartment::multi_threaded().unwrap();
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
        assert_eq!(&*encoder.config(), &[0x11, 0x90]);

        let tone: Vec<i16> = (0..48_000 * 2)
            .map(|index| (f64::from(index / 2) * 0.0576).sin() * 8_000.0)
            .map(|sample| sample as i16)
            .collect();
        let mut frames = Vec::new();
        for chunk in tone.chunks(960) {
            encoder.encode(chunk, |frame| frames.push(frame)).unwrap();
        }
        assert!((44..=47).contains(&frames.len()), "{}", frames.len());
        let bytes: usize = frames.iter().map(|frame| frame.len()).sum();
        assert!(
            (19_000..=29_000).contains(&bytes),
            "{bytes} bytes for a second"
        );
    }

    #[test]
    #[ignore = "needs ffmpeg"]
    fn encoder_delay() {
        let (_com, _media, mut encoder) = encoder().unwrap();
        let mut pcm = vec![0_i16; 48_000 * 2];
        pcm[20_000] = 30_000;
        pcm[20_001] = 30_000;
        let mut frames = Vec::new();
        for chunk in pcm.chunks(960) {
            encoder.encode(chunk, |frame| frames.push(frame)).unwrap();
        }
        let adts = testing::adts(&frames);
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("click.aac");
        std::fs::write(&input, &adts).unwrap();
        let decoded = testing::decode_mono(&input);
        let delay = i64::try_from(testing::peak(&decoded)).unwrap() - 10_000;
        println!(
            "click at 10000, decoded with a delay of {delay}; {} bytes of ADTS, {} samples decoded",
            adts.len(),
            decoded.len()
        );
        assert_eq!(delay, i64::from(PRIMING));
    }
}
