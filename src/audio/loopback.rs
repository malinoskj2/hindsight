//! WASAPI loopback capture of the default output device: a copy of everything the system plays
//! through it, delivered in packets of about 10 ms, each stamped with the performance-counter
//! time of its first sample. The audio thread waits on the capture's event and on its own stop
//! signal, so it sleeps until there is sound to read or it is told to finish.

use super::pcm::{Layout, Sample};
use std::{ffi::c_void, time::Duration};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{HANDLE, WAIT_OBJECT_0},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
                IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE, eConsole, eRender,
            },
            MediaFoundation::{MFAudioFormat_Float, MFAudioFormat_PCM},
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Threading::{CreateEventW, SetEvent, WaitForMultipleObjects},
        },
    },
    core::{Owned, PCWSTR, PWSTR},
};

/// The format tags of float and extensible wave formats, which the audio headers name in
/// modules this one does not otherwise need.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// The bytes an extensible format adds to the basic one.
const EXTENSIBLE_EXTRA_BYTES: u16 = 22;
/// How much sound the capture buffer holds, in 100 ns units: 200 ms, so the thread may be late
/// by many packets before anything is lost.
const BUFFER_DURATION: i64 = 2_000_000;
/// 32-bit float samples, in bits.
const FLOAT_BITS: u16 = 32;

/// COM, initialised for the calling thread in the multithreaded apartment; uninitialised again
/// on drop.
pub(crate) struct Com(());

impl Com {
    pub(crate) fn start() -> windows::core::Result<Self> {
        // SAFETY: a plain call; every success, including "already initialised", is matched by
        // the uninitialise in `drop`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(()))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: matches the successful initialise that made `self`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// An event another thread sets to tell the audio thread to finish.
pub(crate) struct Signal(Owned<HANDLE>);

// SAFETY: an event handle may be waited on and set from any thread; the handle is only closed
// when the last owner drops it.
unsafe impl Send for Signal {}
// SAFETY: as above; setting and waiting are both thread-safe kernel calls.
unsafe impl Sync for Signal {}

impl Signal {
    pub(crate) fn new() -> windows::core::Result<Self> {
        // SAFETY: an unnamed manual-reset event, initially clear, with default security.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        // SAFETY: the event was just created, and only this `Owned` closes it.
        Ok(Self(unsafe { Owned::new(event) }))
    }

    /// Sets the event; it stays set.
    #[expect(
        dead_code,
        reason = "sessions stop the audio thread from the next commits"
    )]
    pub(crate) fn set(&self) {
        // SAFETY: the event is open.
        let _ = unsafe { SetEvent(*self.0) };
    }
}

/// What ended a wait.
pub(crate) enum Wake {
    /// The stop signal is set.
    Stop,
    /// The capture has sound to read, or the timeout passed.
    Read,
}

/// Waits until `stop` is set, `ready` (a capture's event) is, or `timeout` passes.
pub(crate) fn wait(stop: &Signal, ready: Option<HANDLE>, timeout: Duration) -> Wake {
    let mut handles = vec![*stop.0];
    handles.extend(ready);
    let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: both handles are open events for the length of the call.
    let woken = unsafe { WaitForMultipleObjects(&handles, false, milliseconds) };
    if woken == WAIT_OBJECT_0 {
        Wake::Stop
    } else {
        Wake::Read
    }
}

/// Why the default output device could not be captured.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    /// There is no output device: none is connected, or all are disabled.
    #[error("there is no audio output device")]
    NoDevice,
    /// A Windows call failed in the named step.
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
}

/// Wraps a Windows error with the step that failed, for `map_err`.
fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> OpenError {
    move |error| OpenError::Windows(step, error)
}

/// The system's list of audio devices, for opening the default output and noticing when it
/// changes.
pub(crate) struct Devices(IMMDeviceEnumerator);

impl Devices {
    pub(crate) fn new() -> windows::core::Result<Self> {
        // SAFETY: creates the in-process device enumerator; COM is initialised on this thread.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map(Self)
    }

    /// The default output device for games and media.
    fn default_output(&self) -> Result<IMMDevice, OpenError> {
        // SAFETY: a query of the live enumerator.
        unsafe { self.0.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| OpenError::NoDevice)
    }

    /// The id of the default output device, or `None` when there is none.
    pub(crate) fn default_id(&self) -> Option<String> {
        self.default_output().ok().and_then(|device| id(&device))
    }

    /// Starts capturing what the default output device plays.
    pub(crate) fn open(&self) -> Result<Loopback, OpenError> {
        let device = self.default_output()?;
        // SAFETY: activates an audio client on the live device.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(failed("activate the audio client"))?;
        let layout = initialize(&client)?;
        // SAFETY: an unnamed auto-reset event, initially clear, with default security.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(failed("create the audio event"))?;
        // SAFETY: the event was just created, and only this `Owned` closes it.
        let event = unsafe { Owned::new(event) };
        // SAFETY: calls on the initialised client; the event outlives it, since both live in
        // the returned capture and the client is stopped and released first.
        let capture = unsafe {
            client
                .SetEventHandle(*event)
                .map_err(failed("set the audio event"))?;
            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(failed("get the capture client"))?;
            client.Start().map_err(failed("start the audio capture"))?;
            capture
        };
        Ok(Loopback {
            client,
            capture,
            event,
            layout,
            id: id(&device).unwrap_or_default(),
            name: friendly_name(&device).unwrap_or_else(|| "the default output".to_owned()),
        })
    }
}

/// Initialises `client` for shared-mode loopback in the engine's own format when that is 48 kHz
/// float or 16-bit with one or two channels, which needs no conversion by Windows; otherwise it
/// asks Windows to convert to 48 kHz float stereo. Returns the layout of the samples it will
/// deliver.
fn initialize(client: &IAudioClient) -> Result<Layout, OpenError> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    // SAFETY: the mix format is allocated by COM, read while it lives, and freed below.
    let mix = unsafe { client.GetMixFormat() }.map_err(failed("read the audio format"))?;
    let direct = mix_layout(mix);
    // SAFETY: frees the format GetMixFormat allocated, after `initialize_as` has used it.
    let free = || unsafe { CoTaskMemFree(Some(mix.cast_const().cast::<c_void>())) };
    if let Some(layout) = direct {
        let result = initialize_as(client, mix, flags);
        free();
        return result.map(|()| layout);
    }
    free();
    let converted = float_stereo();
    initialize_as(
        client,
        &raw const converted,
        flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    )?;
    Ok(Layout {
        sample: Sample::Float,
        channels: super::CHANNELS,
    })
}

/// Initialises `client` with `format` and `flags`.
fn initialize_as(
    client: &IAudioClient,
    format: *const WAVEFORMATEX,
    flags: u32,
) -> Result<(), OpenError> {
    // SAFETY: `format` points at a complete wave format for the length of the call.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION,
            0,
            format,
            None,
        )
    }
    .map_err(failed("open the audio capture"))
}

/// The layout of the engine's format at `mix`, when it can be captured as it is.
fn mix_layout(mix: *const WAVEFORMATEX) -> Option<Layout> {
    // SAFETY: GetMixFormat returned a valid format; an extensible one is followed by the
    // extensible fields its size declares. Both structures are packed, so they are read
    // unaligned.
    let (basic, sub_format) = unsafe {
        let basic = mix.read_unaligned();
        let sub_format = (basic.wFormatTag == WAVE_FORMAT_EXTENSIBLE
            && basic.cbSize >= EXTENSIBLE_EXTRA_BYTES)
            .then(|| {
                mix.cast::<WAVEFORMATEXTENSIBLE>()
                    .read_unaligned()
                    .SubFormat
            });
        (basic, sub_format)
    };
    let float = match (basic.wFormatTag, sub_format) {
        (WAVE_FORMAT_IEEE_FLOAT, _) => true,
        (tag, _) if u32::from(tag) == WAVE_FORMAT_PCM => false,
        (WAVE_FORMAT_EXTENSIBLE, Some(format)) if format == MFAudioFormat_Float => true,
        (WAVE_FORMAT_EXTENSIBLE, Some(format)) if format == MFAudioFormat_PCM => false,
        _ => return None,
    };
    let sample = match (float, basic.wBitsPerSample) {
        (true, FLOAT_BITS) => Sample::Float,
        (false, 16) => Sample::Int16,
        _ => return None,
    };
    let channels = basic.nChannels;
    (basic.nSamplesPerSec == super::SAMPLE_RATE && (1..=2).contains(&channels))
        .then_some(Layout { sample, channels })
}

/// 48 kHz 32-bit float stereo, the format Windows is asked to convert to.
fn float_stereo() -> WAVEFORMATEX {
    let block = super::CHANNELS * FLOAT_BITS / 8;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: super::CHANNELS,
        nSamplesPerSec: super::SAMPLE_RATE,
        nAvgBytesPerSec: super::SAMPLE_RATE * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: FLOAT_BITS,
        cbSize: 0,
    }
}

/// The device's id, which stays the same while it is the same device.
fn id(device: &IMMDevice) -> Option<String> {
    // SAFETY: the id is allocated by COM, copied, and freed.
    unsafe {
        let id = device.GetId().ok()?;
        Some(take_string(id))
    }
}

/// The device's name as the Sound settings show it, such as "Speakers (Realtek(R) Audio)".
fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: the property value is read, converted into a COM-allocated string that is
    // copied and freed, and cleared.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToStringAlloc(&raw const value).ok();
        let _ = PropVariantClear(&raw mut value);
        name.map(|name| take_string(name))
    }
}

/// Copies a COM-allocated string and frees it.
///
/// # Safety
///
/// `text` must be a valid string allocated with `CoTaskMemAlloc`, which is not used again.
unsafe fn take_string(text: PWSTR) -> String {
    // SAFETY: the caller hands over a valid string, freed here once copied.
    unsafe {
        let copied = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0.cast_const().cast::<c_void>()));
        copied
    }
}

/// One packet of captured sound.
pub(crate) struct Packet<'a> {
    /// The samples, or `None` when the packet is silence.
    pub(crate) data: Option<&'a [u8]>,
    /// Samples per channel in the packet.
    pub(crate) frames: u32,
    /// The clock time of its first sample, when Windows could tell.
    pub(crate) time: Option<i64>,
}

/// A running loopback capture of one device.
pub(crate) struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Owned<HANDLE>,
    layout: Layout,
    id: String,
    name: String,
}

impl Loopback {
    /// The event set when sound is ready to read.
    pub(crate) fn event(&self) -> HANDLE {
        *self.event
    }

    /// The layout of the samples in every packet.
    pub(crate) const fn layout(&self) -> Layout {
        self.layout
    }

    /// The device's id.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The device's name, for the log.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Hands every packet waiting in the capture buffer to `each`, oldest first. An error from
    /// Windows, such as `AUDCLNT_E_DEVICE_INVALIDATED` when the device goes away, ends the
    /// reading, as does one from `each`.
    pub(crate) fn read<E: From<windows::core::Error>>(
        &mut self,
        mut each: impl FnMut(&Packet<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let frame_bytes = self.layout.frame_bytes();
        // SAFETY: each packet is taken from the live capture client and released once `each`
        // has looked at it; the slice made from it does not outlive the release.
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                let mut time = 0;
                self.capture.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    None,
                    Some(&raw mut time),
                )?;
                let silent =
                    flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0 || data.is_null();
                let stamped = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0.cast_unsigned() == 0;
                let packet = Packet {
                    data: (!silent)
                        .then(|| std::slice::from_raw_parts(data, frames as usize * frame_bytes)),
                    frames,
                    time: stamped.then(|| i64::try_from(time).unwrap_or(i64::MAX)),
                };
                let handled = each(&packet);
                self.capture.ReleaseBuffer(frames)?;
                handled?;
            }
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stops the client before it and the event it signals are released.
        let _ = unsafe { self.client.Stop() };
    }
}
