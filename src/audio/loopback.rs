//! WASAPI loopback capture of the default output device, read on a timer a few packets at a time.

use super::pcm::{Layout, Sample};
use listener::Listener;
use std::{ffi::c_void, fmt, sync::Arc, time::Duration};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{HANDLE, PROPERTYKEY, WAIT_OBJECT_0},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                DEVICE_STATE, EDataFlow, ERole, IAudioCaptureClient, IAudioClient, IMMDevice,
                IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
                MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole,
                eRender,
            },
            MediaFoundation::{MFAudioFormat_Float, MFAudioFormat_PCM},
        },
        System::{
            Com::{
                CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Threading::{
                CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{Owned, PCWSTR, PWSTR, implement},
};

/// Format tags the audio headers put in modules this one does not otherwise need.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const EXTENSIBLE_EXTRA_BYTES: u16 = 22;
/// 200 ms in 100 ns units; a late read loses nothing.
const BUFFER_DURATION: i64 = 2_000_000;
const FLOAT_BITS: u16 = 32;

pub(super) struct Signal(Owned<HANDLE>);

// SAFETY: event handles may be set and waited on from any thread; closed only by the last owner.
unsafe impl Send for Signal {}
// SAFETY: as above.
unsafe impl Sync for Signal {}

impl Signal {
    pub(super) fn new() -> windows::core::Result<Self> {
        // SAFETY: an unnamed manual-reset event, initially clear, with default security.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        // SAFETY: the event was just created, and only this `Owned` closes it.
        Ok(Self(unsafe { Owned::new(event) }))
    }

    pub(super) fn set(&self) {
        // SAFETY: the event is open.
        let _ = unsafe { SetEvent(*self.0) };
    }

    fn take(&self) -> bool {
        // SAFETY: the event is open; a zero timeout only looks at its state.
        let set = unsafe { WaitForSingleObject(*self.0, 0) } == WAIT_OBJECT_0;
        if set {
            // SAFETY: the event is open.
            let _ = unsafe { ResetEvent(*self.0) };
        }
        set
    }
}

pub(super) enum Wake {
    Stop,
    Read,
}

pub(super) fn wait(stop: &Signal, wake: Option<HANDLE>, timeout: Duration) -> Wake {
    let handles = [*stop.0, wake.unwrap_or_default()];
    let handles = &handles[..=usize::from(wake.is_some())];
    let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the handles waited on are open events for the length of the call.
    let woken = unsafe { WaitForMultipleObjects(handles, false, milliseconds) };
    if woken == WAIT_OBJECT_0 {
        Wake::Stop
    } else {
        Wake::Read
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OpenError {
    #[error("there is no audio output device")]
    NoDevice,
    #[error("{0}: {1}")]
    Windows(&'static str, #[source] windows::core::Error),
}

fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> OpenError {
    move |error| OpenError::Windows(step, error)
}

pub(super) struct Devices(IMMDeviceEnumerator);

impl Devices {
    pub(super) fn new() -> windows::core::Result<Self> {
        // SAFETY: creates the in-process device enumerator; COM is initialised on this thread.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map(Self)
    }

    fn default_output(&self) -> Result<IMMDevice, OpenError> {
        // SAFETY: a query of the live enumerator.
        unsafe { self.0.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| OpenError::NoDevice)
    }

    pub(super) fn default_id(&self) -> Option<String> {
        self.default_output().ok().and_then(|device| id(&device))
    }

    pub(super) fn open(&self) -> Result<Loopback, OpenError> {
        let device = self.default_output()?;
        // SAFETY: activates an audio client on the live device.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(failed("activate the audio client"))?;
        let layout = initialize(&client)?;
        // SAFETY: calls on the initialised client.
        let capture = unsafe {
            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(failed("get the capture client"))?;
            client.Start().map_err(failed("start the audio capture"))?;
            capture
        };
        Ok(Loopback {
            client,
            capture,
            layout,
            id: id(&device).unwrap_or_default(),
            name: friendly_name(&device).unwrap_or_else(|| "the default output".to_owned()),
        })
    }

    pub(super) fn watch(&self) -> windows::core::Result<Watch> {
        let changed = Arc::new(Signal::new()?);
        let listener: IMMNotificationClient = Listener(Arc::clone(&changed)).into();
        // SAFETY: `Watch` unregisters the listener before either is released.
        unsafe { self.0.RegisterEndpointNotificationCallback(&listener) }?;
        Ok(Watch {
            devices: self.0.clone(),
            listener,
            changed,
        })
    }
}

pub(super) struct Watch {
    devices: IMMDeviceEnumerator,
    listener: IMMNotificationClient,
    changed: Arc<Signal>,
}

impl Watch {
    pub(super) fn event(&self) -> HANDLE {
        *self.changed.0
    }

    pub(super) fn changed(&self) -> bool {
        self.changed.take()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: unregisters the listener this watch registered, while both are still alive.
        let _ = unsafe {
            self.devices
                .UnregisterEndpointNotificationCallback(&self.listener)
        };
    }
}

/// A module of its own so the `implement` macro's glue can be exempted from two pedantic lints.
mod listener {
    #![allow(clippy::ref_as_ptr, clippy::inline_always)]

    use super::{
        DEVICE_STATE, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl, PCWSTR,
        PROPERTYKEY, Signal, eConsole, eRender, implement,
    };
    use std::sync::Arc;

    /// Only the default device changing matters; one lost while open shows as a capture failure.
    #[implement(IMMNotificationClient)]
    pub(super) struct Listener(pub(super) Arc<Signal>);

    impl IMMNotificationClient_Impl for Listener_Impl {
        fn OnDeviceStateChanged(
            &self,
            _id: &PCWSTR,
            _state: DEVICE_STATE,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }

        fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            _id: &PCWSTR,
        ) -> windows::core::Result<()> {
            if flow == eRender && role == eConsole {
                self.0.set();
            }
            Ok(())
        }

        fn OnPropertyValueChanged(
            &self,
            _id: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }
}

/// Memory a COM call allocated for the caller to free.
struct CoTaskMem<T>(*mut T);

impl<T> Drop for CoTaskMem<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer is the one the COM call handed over, freed once.
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast::<c_void>())) };
    }
}

impl fmt::Display for CoTaskMem<u16> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: the memory holds a null-terminated string.
        let text = unsafe { PWSTR(self.0).to_string() }.unwrap_or_default();
        formatter.write_str(&text)
    }
}

fn initialize(client: &IAudioClient) -> Result<Layout, OpenError> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK;
    // SAFETY: a query of the live client; the format it allocates is freed with `mix`.
    let mix = CoTaskMem(unsafe { client.GetMixFormat() }.map_err(failed("read the audio format"))?);
    if let Some(layout) = mix_layout(mix.0) {
        initialize_as(client, mix.0, flags)?;
        return Ok(layout);
    }
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

fn mix_layout(mix: *const WAVEFORMATEX) -> Option<Layout> {
    // SAFETY: GetMixFormat returned a valid format, extended by the fields `cbSize` declares when
    // extensible; both structures are packed, so they are read unaligned.
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

fn id(device: &IMMDevice) -> Option<String> {
    // SAFETY: a query of the live device; the id it allocates is freed with `CoTaskMem`.
    let id = unsafe { device.GetId() }.ok()?;
    Some(CoTaskMem(id.0).to_string())
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: the value is read from the live device's store and cleared once converted into a
    // string, which is freed with `CoTaskMem`.
    let name = unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToStringAlloc(&raw const value).ok();
        let _ = PropVariantClear(&raw mut value);
        name
    }?;
    Some(CoTaskMem(name.0).to_string())
}

pub(super) struct Packet<'a> {
    /// `None` for a silent packet.
    pub(super) data: Option<&'a [u8]>,
    pub(super) frames: u32,
    /// Performance-counter time of the first sample, when stamped.
    pub(super) time: Option<i64>,
}

pub(super) struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    layout: Layout,
    id: String,
    name: String,
}

impl Loopback {
    pub(super) const fn layout(&self) -> Layout {
        self.layout
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn read(
        &mut self,
        mut each: impl FnMut(&Packet<'_>) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        let frame_bytes = self.layout.frame_bytes();
        // SAFETY: a query of the running capture client.
        while unsafe { self.capture.GetNextPacketSize() }? > 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut time = 0;
            // SAFETY: the out-pointers are to locals; the buffer stays valid until released below.
            unsafe {
                self.capture.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    None,
                    Some(&raw mut time),
                )
            }?;
            let silent =
                flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0 || data.is_null();
            let stamped = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0.cast_unsigned() == 0;
            let packet = Packet {
                // SAFETY: the buffer holds `frames` frames of `frame_bytes`, live until released.
                data: (!silent).then(|| unsafe {
                    std::slice::from_raw_parts(data, frames as usize * frame_bytes)
                }),
                frames,
                time: stamped.then(|| i64::try_from(time).unwrap_or(i64::MAX)),
            };
            let handled = each(&packet);
            // SAFETY: releases the buffer taken above, once `packet` no longer refers to it.
            unsafe { self.capture.ReleaseBuffer(frames) }?;
            handled?;
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stops the client before it is released.
        let _ = unsafe { self.client.Stop() };
    }
}
