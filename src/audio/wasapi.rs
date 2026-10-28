//! WASAPI capture of an output device, by loopback, and of a microphone, read on a timer a few
//! packets at a time, and the devices there are.

use super::{
    choice::{self, Endpoint, Picked},
    pcm::{Layout, Sample},
    timeline::Ticks,
};
use crate::{config::DeviceId, replay::Source};
use listener::Listener;
use std::{ptr, sync::Arc, time::Duration};
use task_mem::CoTaskMem;
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{
            E_UNEXPECTED, HANDLE, PROPERTYKEY, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioCaptureClient,
                IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
                IMMNotificationClient_Impl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
            },
            MediaFoundation::{MFAudioFormat_Float, MFAudioFormat_PCM},
        },
        System::{
            Com::{
                CLSCTX_ALL, CoCreateInstance, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Threading::WaitForMultipleObjects,
        },
    },
    core::{GUID, HRESULT, HSTRING, PCWSTR, implement},
};
use winsafe::{HEVENT, guard::CloseHandleGuard};

/// Format tags from the audio headers that live in modules this one does not otherwise need.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const EXTENSIBLE_EXTRA_BYTES: u16 = 22;
/// 200 ms in 100 ns units. A late read loses nothing.
const BUFFER_DURATION: i64 = 2_000_000;
const FLOAT_BITS: u16 = 32;

/// An event handle. It is `Send` through winsafe: a kernel event handle is not tied to the thread
/// that opened it.
pub(super) struct Signal(CloseHandleGuard<HEVENT>);

// SAFETY: event handles may be set and waited on from any thread at once, and the handle is
// closed only by the last owner.
unsafe impl Sync for Signal {}

impl Signal {
    /// Creates an event that stays set once set, so that every wait after a stop request sees it.
    pub(super) fn latch() -> windows::core::Result<Self> {
        Self::new(true)
    }

    /// Creates an event that is cleared by the wait it wakes, so that each set is seen once.
    fn pulse() -> windows::core::Result<Self> {
        Self::new(false)
    }

    /// Creates an unnamed event with default security, initially clear.
    fn new(manual_reset: bool) -> windows::core::Result<Self> {
        HEVENT::CreateEvent(None, manual_reset, false, None)
            .map(Self)
            .map_err(|error| HRESULT::from_win32(error.raw()).into())
    }

    pub(super) fn set(&self) {
        let _ = self.0.SetEvent();
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0.ptr())
    }
}

pub(super) enum Wake {
    Stop,
    /// A device came or went, or a default device changed.
    Changed,
    /// The timeout elapsed, so it is time to read.
    Read,
}

/// The second handle `wait` waits on.
const CHANGED: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);

/// Waits for `stop`, the device-change event `changed` or the timeout. A device change is
/// reported once: its event clears as the wait returns. An error means waiting itself failed.
pub(super) fn wait(
    stop: &Signal,
    changed: Option<HANDLE>,
    timeout: Duration,
) -> windows::core::Result<Wake> {
    let handles = [stop.handle(), changed.unwrap_or_default()];
    let handles = &handles[..=usize::from(changed.is_some())];
    let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: both handles are open events for the length of the call: `stop` is borrowed and
    // `changed` belongs to a `Watch` the caller holds.
    let woken = unsafe { WaitForMultipleObjects(handles, false, milliseconds) };
    match woken {
        WAIT_OBJECT_0 => Ok(Wake::Stop),
        CHANGED => Ok(Wake::Changed),
        WAIT_TIMEOUT => Ok(Wake::Read),
        WAIT_FAILED => Err(windows::core::Error::from_thread()),
        // Events are never abandoned and only two handles are waited on.
        WAIT_EVENT(other) => Err(windows::core::Error::new(
            E_UNEXPECTED,
            format!("unexpected wait result {other:#X}"),
        )),
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OpenError {
    #[error("there is no device")]
    NoDevice,
    #[error("could not {0}")]
    Windows(&'static str, #[source] windows::core::Error),
}

fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> OpenError {
    move |error| OpenError::Windows(step, error)
}

/// The endpoint each source is captured from: what an output device plays is captured by
/// loopback.
const fn flow(source: Source) -> EDataFlow {
    match source {
        Source::System => eRender,
        Source::Microphone => eCapture,
    }
}

const fn fallback_name(source: Source) -> &'static str {
    match source {
        Source::System => "the output device",
        Source::Microphone => "the microphone",
    }
}

pub(super) struct Devices(IMMDeviceEnumerator);

/// A device picked for a source, not yet opened.
pub(super) struct Pick {
    device: IMMDevice,
    picked: Picked,
}

impl Pick {
    pub(super) fn id(&self) -> Option<String> {
        id(&self.device)
    }

    pub(super) const fn picked(&self) -> Picked {
        self.picked
    }

    /// Opens the device and starts capturing `source` from it.
    pub(super) fn open(self, source: Source) -> Result<Stream, OpenError> {
        let Self { device, picked } = self;
        // SAFETY: activates an audio client on the live device.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(failed("activate the audio client"))?;
        let layout = initialize(&client, source)?;
        // SAFETY: the client is initialised, which a capture service requires. The call returns a
        // new reference.
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.map_err(failed("get the capture client"))?;
        // SAFETY: the client is initialised, which starting requires.
        unsafe { client.Start() }.map_err(failed("start the audio capture"))?;
        Ok(Stream {
            client,
            capture,
            layout,
            id: id(&device).unwrap_or_default(),
            name: friendly_name(&device).unwrap_or_else(|| fallback_name(source).to_owned()),
            picked,
        })
    }
}

impl Devices {
    pub(super) fn new() -> windows::core::Result<Self> {
        // SAFETY: creates the in-process device enumerator; COM is initialised on this thread.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map(Self)
    }

    fn default_device(&self, source: Source) -> Result<IMMDevice, OpenError> {
        // SAFETY: a query of the live enumerator.
        unsafe { self.0.GetDefaultAudioEndpoint(flow(source), eConsole) }
            .map_err(|_| OpenError::NoDevice)
    }

    /// Returns the device with endpoint ID `id`, connected or not, if Windows knows it.
    fn device(&self, id: &DeviceId) -> Option<IMMDevice> {
        let id = HSTRING::from(id.as_str());
        // SAFETY: a query of the live enumerator; `id` is a null-terminated string that outlives
        // the call.
        unsafe { self.0.GetDevice(&id) }.ok()
    }

    /// Returns the device with endpoint ID `id` if it is connected and enabled.
    fn active(&self, id: &DeviceId) -> Option<IMMDevice> {
        let device = self.device(id)?;
        // SAFETY: a query of a live device.
        let state = unsafe { device.GetState() }.ok()?;
        (state == DEVICE_STATE_ACTIVE).then_some(device)
    }

    /// Picks the device `choice` names if it is present, or else the default for `source`.
    pub(super) fn pick(
        &self,
        source: Source,
        choice: Option<&DeviceId>,
    ) -> Result<Pick, OpenError> {
        let (device, picked) =
            choice::pick(choice, |id| self.active(id), || self.default_device(source))?;
        Ok(Pick { device, picked })
    }

    /// Returns the connected, enabled devices `source` can be captured from.
    pub(super) fn endpoints(&self, source: Source) -> windows::core::Result<Vec<Endpoint>> {
        // SAFETY: a query of the live enumerator; the call returns a new collection.
        let collection = unsafe { self.0.EnumAudioEndpoints(flow(source), DEVICE_STATE_ACTIVE) }?;
        // SAFETY: a query of the live collection.
        let count = unsafe { collection.GetCount() }?;
        let mut endpoints = Vec::new();
        for index in 0..count {
            // SAFETY: `index` is below the collection's count; the call returns a new reference.
            let device = unsafe { collection.Item(index) }?;
            let Some(id) = id(&device).and_then(DeviceId::new) else {
                continue;
            };
            let name = friendly_name(&device).unwrap_or_else(|| id.as_str().to_owned());
            endpoints.push(Endpoint { id, name });
        }
        Ok(endpoints)
    }

    /// Returns the name of the device with endpoint ID `id`, connected or not, if Windows knows
    /// it.
    pub(super) fn name(&self, id: &DeviceId) -> Option<String> {
        friendly_name(&self.device(id)?)
    }

    /// Calls `changed`, on a thread of Windows', whenever a device comes or goes, is enabled or
    /// disabled, or becomes a default device, until the registration drops.
    pub(super) fn register(
        &self,
        changed: impl Fn() + Send + Sync + 'static,
    ) -> windows::core::Result<Registration> {
        let listener: IMMNotificationClient = Listener(Box::new(changed)).into();
        // SAFETY: `Registration` unregisters the listener before either is released.
        unsafe { self.0.RegisterEndpointNotificationCallback(&listener) }?;
        Ok(Registration {
            devices: self.0.clone(),
            listener,
        })
    }

    /// Watches for the changes `register` reports, as an event `wait` can wait on.
    pub(super) fn watch(&self) -> windows::core::Result<Watch> {
        let changed = Arc::new(Signal::pulse()?);
        let signal = Arc::clone(&changed);
        let registration = self.register(move || signal.set())?;
        Ok(Watch {
            _registration: registration,
            changed,
        })
    }
}

pub(super) struct Registration {
    devices: IMMDeviceEnumerator,
    listener: IMMNotificationClient,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: unregisters the listener this registration registered, while both are alive.
        let _ = unsafe {
            self.devices
                .UnregisterEndpointNotificationCallback(&self.listener)
        };
    }
}

/// The event pulsed when a device changes, and the registration that pulses it. The listener
/// holds the signal through an `Arc` of its own, so neither field need drop first.
pub(super) struct Watch {
    _registration: Registration,
    changed: Arc<Signal>,
}

impl Watch {
    /// Returns the event set when a device changes. `wait` reports it as `Wake::Changed`.
    pub(super) fn event(&self) -> HANDLE {
        self.changed.handle()
    }
}

/// A module of its own so the `implement` macro's glue can be exempted from two pedantic lints.
mod listener {
    #![allow(
        clippy::ref_as_ptr,
        clippy::inline_always,
        reason = "the `implement` macro's glue, which this module cannot change"
    )]

    use super::{
        DEVICE_STATE, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl, PCWSTR,
        PROPERTYKEY, eCapture, eConsole, eRender, implement,
    };

    /// Calls its function when a device comes or goes, is enabled or disabled, or becomes the
    /// default output or recording device. A change of a device's properties does not count.
    #[implement(IMMNotificationClient)]
    pub(super) struct Listener(pub(super) Box<dyn Fn() + Send + Sync>);

    /// Windows calls the listener on threads of its own.
    const _: () = {
        const fn send_and_sync<T: Send + Sync>() {}
        send_and_sync::<Listener>();
        send_and_sync::<Listener_Impl>();
    };

    impl IMMNotificationClient_Impl for Listener_Impl {
        fn OnDeviceStateChanged(
            &self,
            _id: &PCWSTR,
            _state: DEVICE_STATE,
        ) -> windows::core::Result<()> {
            (self.0)();
            Ok(())
        }

        fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
            (self.0)();
            Ok(())
        }

        fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
            (self.0)();
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            _id: &PCWSTR,
        ) -> windows::core::Result<()> {
            if (flow == eRender || flow == eCapture) && role == eConsole {
                (self.0)();
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

/// A module of its own so the pointer can only be wrapped through `from_raw`.
mod task_mem {
    use super::{EXTENSIBLE_EXTRA_BYTES, WAVE_FORMAT_EXTENSIBLE, WaveFormat};
    use std::ffi::c_void;
    use windows::{
        Win32::{
            Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE},
            System::Com::CoTaskMemFree,
        },
        core::PWSTR,
    };

    /// Memory a COM call allocated for the caller to free.
    pub(super) struct CoTaskMem<T>(*mut T);

    impl<T> CoTaskMem<T> {
        /// # Safety
        ///
        /// `pointer` is null, or was allocated with `CoTaskMemAlloc`, is freed by nothing else,
        /// and points at a valid `T` for as long as the result lives. A wave format is followed
        /// by the `cbSize` bytes it declares; a string is null-terminated.
        pub(super) const unsafe fn from_raw(pointer: *mut T) -> Self {
            Self(pointer)
        }
    }

    impl<T> Drop for CoTaskMem<T> {
        fn drop(&mut self) {
            // SAFETY: by `from_raw`'s contract the pointer is null, which is ignored, or memory
            // from `CoTaskMemAlloc` that only `self` frees.
            unsafe { CoTaskMemFree(Some(self.0.cast_const().cast::<c_void>())) };
        }
    }

    impl CoTaskMem<u16> {
        /// Returns the string held, with invalid UTF-16 replaced by U+FFFD, or `None` when the
        /// pointer is null.
        pub(super) fn to_string_lossy(&self) -> Option<String> {
            if self.0.is_null() {
                return None;
            }
            let text = PWSTR(self.0);
            // SAFETY: by `from_raw`'s contract the non-null pointer is a null-terminated string,
            // alive and unchanged while `self` is borrowed.
            Some(String::from_utf16_lossy(unsafe { text.as_wide() }))
        }
    }

    impl CoTaskMem<WAVEFORMATEX> {
        /// Returns a copy of the format held, or `None` when the pointer is null.
        pub(super) fn to_wave_format(&self) -> Option<WaveFormat> {
            let format = self.0.cast_const();
            if format.is_null() {
                return None;
            }
            // SAFETY: by `from_raw`'s contract the non-null pointer is a valid wave format. The
            // structure is packed, so it is read unaligned.
            let basic = unsafe { format.read_unaligned() };
            if basic.wFormatTag != WAVE_FORMAT_EXTENSIBLE || basic.cbSize < EXTENSIBLE_EXTRA_BYTES {
                return Some(WaveFormat::plain(basic));
            }
            let extensible = format.cast::<WAVEFORMATEXTENSIBLE>();
            // SAFETY: `cbSize` declares the extensible fields, which by `from_raw`'s contract
            // follow the basic ones in the same allocation. The structure is packed, so it is read
            // unaligned.
            Some(WaveFormat::extensible(unsafe {
                extensible.read_unaligned()
            }))
        }
    }
}

fn initialize(client: &IAudioClient, source: Source) -> Result<Layout, OpenError> {
    let flags = match source {
        Source::System => AUDCLNT_STREAMFLAGS_LOOPBACK,
        Source::Microphone => 0,
    };
    // SAFETY: a query of the live client.
    let mix = unsafe { client.GetMixFormat() }.map_err(failed("read the audio format"))?;
    // SAFETY: GetMixFormat allocates the format with `CoTaskMemAlloc` for the caller to free,
    // followed by the extra bytes its `cbSize` declares.
    let mix = unsafe { CoTaskMem::from_raw(mix) }.to_wave_format();
    if let Some(mix) = mix
        && let Some(layout) = mix.layout()
    {
        initialize_as(client, &mix, flags)?;
        return Ok(layout);
    }
    initialize_as(
        client,
        &WaveFormat::float_stereo(),
        flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    )?;
    Ok(Layout {
        sample: Sample::Float,
        channels: super::CHANNELS.get(),
    })
}

fn initialize_as(client: &IAudioClient, format: &WaveFormat, flags: u32) -> Result<(), OpenError> {
    // SAFETY: `format` is borrowed for the call and is a whole wave format: its `cbSize` is the
    // size of the fields its variant holds, so the client reads nothing past it.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION,
            0,
            format.as_ptr(),
            None,
        )
    }
    .map_err(failed("open the audio capture"))
}

/// A wave format owned here, copied out of the memory a COM call returned. Its `cbSize` always
/// matches the variant, so a pointer to it is a complete format.
#[derive(Clone, Copy)]
enum WaveFormat {
    Plain(WAVEFORMATEX),
    Extensible(WAVEFORMATEXTENSIBLE),
}

impl WaveFormat {
    /// Wraps a plain format. Any extra bytes `format` declares are dropped.
    const fn plain(mut format: WAVEFORMATEX) -> Self {
        format.cbSize = 0;
        Self::Plain(format)
    }

    /// Wraps an extensible format. Any bytes past the extensible fields are dropped.
    const fn extensible(mut format: WAVEFORMATEXTENSIBLE) -> Self {
        format.Format.cbSize = EXTENSIBLE_EXTRA_BYTES;
        Self::Extensible(format)
    }

    fn float_stereo() -> Self {
        let block = super::CHANNELS.get() * FLOAT_BITS / 8;
        Self::plain(WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: super::CHANNELS.get(),
            nSamplesPerSec: super::SAMPLE_RATE.get(),
            nAvgBytesPerSec: super::SAMPLE_RATE.get() * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: FLOAT_BITS,
            cbSize: 0,
        })
    }

    const fn basic(&self) -> WAVEFORMATEX {
        match self {
            Self::Plain(format) => *format,
            Self::Extensible(format) => format.Format,
        }
    }

    const fn sub_format(&self) -> Option<GUID> {
        match self {
            Self::Plain(_) => None,
            Self::Extensible(format) => Some(format.SubFormat),
        }
    }

    fn as_ptr(&self) -> *const WAVEFORMATEX {
        match self {
            Self::Plain(format) => ptr::from_ref(format),
            Self::Extensible(format) => ptr::from_ref(format).cast(),
        }
    }

    /// Returns the layout of captured packets in this format, if the capture can use the format
    /// as it is.
    fn layout(&self) -> Option<Layout> {
        let basic = self.basic();
        let float = match (basic.wFormatTag, self.sub_format()) {
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
        (basic.nSamplesPerSec == super::SAMPLE_RATE.get() && (1..=2).contains(&channels))
            .then_some(Layout { sample, channels })
    }
}

fn id(device: &IMMDevice) -> Option<String> {
    // SAFETY: a query of a live device the caller holds.
    let id = unsafe { device.GetId() }.ok()?;
    // SAFETY: GetId allocates a null-terminated string with `CoTaskMemAlloc` for the caller.
    unsafe { CoTaskMem::from_raw(id.0) }.to_string_lossy()
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: a query of a live device the caller holds; the call returns a new reference.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    // SAFETY: the key is borrowed for the call; the value returned is owned here.
    let mut value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.ok()?;
    // SAFETY: `value` is an initialised PROPVARIANT, borrowed for the call.
    let name = unsafe { PropVariantToStringAlloc(&raw const value) }.ok();
    // SAFETY: `value` is initialised and cleared once, after its last read above.
    let _ = unsafe { PropVariantClear(&raw mut value) };
    let name = name?;
    // SAFETY: PropVariantToStringAlloc allocates a null-terminated string with
    // `CoTaskMemAlloc` for the caller.
    unsafe { CoTaskMem::from_raw(name.0) }.to_string_lossy()
}

pub(super) struct Packet<'a> {
    /// `None` for a silent packet.
    pub(super) data: Option<&'a [u8]>,
    pub(super) frames: u32,
    /// Performance-counter time of the first frame, when stamped.
    pub(super) time: Option<Ticks>,
}

/// A started capture stream of one device.
pub(super) struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    layout: Layout,
    id: String,
    name: String,
    picked: Picked,
}

impl Stream {
    pub(super) const fn picked(&self) -> Picked {
        self.picked
    }

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
                time: stamped.then(|| Ticks(i64::try_from(time).unwrap_or(i64::MAX))),
            };
            let handled = each(&packet);
            // SAFETY: releases the buffer taken above, once `packet` no longer refers to it.
            unsafe { self.capture.ReleaseBuffer(frames) }?;
            handled?;
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stops the client before it is released.
        let _ = unsafe { self.client.Stop() };
    }
}
