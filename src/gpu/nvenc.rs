//! Hardware encoding through NVIDIA's Video Codec SDK (`nvEncodeAPI64.dll`, installed with the
//! driver).
//!
//! The encoder runs asynchronously over a small ring of slots, each an input texture registered
//! with NVENC, an output buffer and a completion event. The capture thread fills a slot and
//! submits it, then picks up whichever earlier slots the GPU has finished; it only ever waits
//! when every slot is still in flight. Frames are encoded straight from video memory, and only
//! the compressed bitstream is copied to system memory.

use crate::config::Codec;
use std::{collections::VecDeque, ffi::CStr, ptr};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::{
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
                ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
        },
        System::{
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::{GUID, Interface, s, w},
};

mod ffi;

use ffi::{Status, Zeroed};

/// Frames in flight at once. Two would do while the GPU keeps up; the third absorbs a frame
/// that NVENC finishes late because a game saturates the GPU.
const SLOTS: usize = 3;
/// How long to wait for a frame before deciding NVENC has stopped responding.
const ENCODE_TIMEOUT_MS: u32 = 2000;
/// One keyframe per second keeps saved clips within a second of the requested length.
const GOP_SECONDS: u32 = 1;

#[derive(Debug, Error)]
pub(crate) enum NvencError {
    #[error("nvEncodeAPI64.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error(
        "the NVIDIA driver supports NVENC API {0}.{1}; update the driver to use API 12.1 or newer"
    )]
    DriverTooOld(u32, u32),
    #[error("the NVIDIA driver rejected the NVENC API 12.1 structures; update the driver")]
    VersionRejected,
    #[error("this GPU's NVENC has no {0} encoder")]
    Unsupported(Codec),
    #[error(
        "the display is {width}×{height}, larger than the {max_width}×{max_height} this GPU's {codec} encoder accepts"
    )]
    TooLarge {
        codec: Codec,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
    #[error("{call} failed with {status}{detail}")]
    Call {
        call: &'static str,
        status: &'static str,
        detail: String,
    },
    #[error("NVENC did not finish a frame within {ENCODE_TIMEOUT_MS} ms")]
    Timeout,
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
}

/// What to encode.
#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    pub(super) codec: Codec,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) frame_rate: u32,
    pub(super) bitrate: u32,
}

/// A finished frame.
pub(super) struct Encoded {
    pub(super) index: u64,
    pub(super) fresh: bool,
    pub(super) picture_type: u32,
    pub(super) data: Box<[u8]>,
}

impl Encoded {
    /// Whether NVENC reports the frame as an instantaneous decoder refresh.
    pub(super) const fn is_idr(&self) -> bool {
        self.picture_type == ffi::PIC_TYPE_IDR
    }
}

struct Frame {
    index: u64,
    fresh: bool,
    mapped: *mut std::ffi::c_void,
}

struct Slot {
    texture: ID3D11Texture2D,
    registered: *mut std::ffi::c_void,
    bitstream: *mut std::ffi::c_void,
    event: HANDLE,
    in_flight: Option<Frame>,
}

pub(super) struct Encoder {
    api: Box<ffi::FunctionList>,
    session: ffi::Session,
    settings: Settings,
    slots: Vec<Slot>,
    /// Slots submitted and not yet collected, oldest first.
    queue: VecDeque<usize>,
    /// The slot the next frame goes into.
    next: usize,
    /// The slot of the newest submitted frame, whose texture holds the latest picture.
    latest: Option<usize>,
    /// The device's lock, held around calls that touch its resources: the capture frame pool
    /// works on the same device from its own threads.
    device_lock: ID3D11Multithread,
    /// NVENC stopped answering; teardown does not wait for it again.
    stalled: bool,
    library: HMODULE,
}

/// Holds the device lock while it lives.
struct Locked<'a>(&'a ID3D11Multithread);

impl<'a> Locked<'a> {
    fn new(lock: &'a ID3D11Multithread) -> Self {
        // SAFETY: a re-entrant lock on a live device, released in `Drop`.
        unsafe { lock.Enter() };
        Self(lock)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: releases the `Enter` in `new`.
        unsafe { self.0.Leave() };
    }
}

impl Encoder {
    /// Opens an encode session on `device` and prepares its slots.
    pub(super) fn open(device: &ID3D11Device, settings: Settings) -> Result<Self, NvencError> {
        let device_lock: ID3D11Multithread = device
            .cast()
            .map_err(|error| NvencError::Windows("lock the device", error))?;
        let library = load_library()?;
        let mut encoder = Self {
            api: Box::new(ffi::FunctionList::zeroed()),
            session: ptr::null_mut(),
            settings,
            slots: Vec::with_capacity(SLOTS),
            queue: VecDeque::with_capacity(SLOTS),
            next: 0,
            latest: None,
            device_lock,
            stalled: false,
            library,
        };
        // From here on, `Drop` releases whatever has been created when a step fails.
        encoder.connect(device)?;
        encoder.require_codec()?;
        encoder.require_size()?;
        encoder.initialize()?;
        for _ in 0..SLOTS {
            let slot = encoder.create_slot(device)?;
            encoder.slots.push(slot);
        }
        Ok(encoder)
    }

    pub(super) const fn settings(&self) -> Settings {
        self.settings
    }

    fn connect(&mut self, device: &ID3D11Device) -> Result<(), NvencError> {
        // SAFETY: both symbols are looked up in the loaded NVENC library and have the
        // documented signatures.
        let (max_version, create_instance) = unsafe {
            let max_version = GetProcAddress(self.library, s!("NvEncodeAPIGetMaxSupportedVersion"))
                .ok_or(NvencError::Library)?;
            let create_instance = GetProcAddress(self.library, s!("NvEncodeAPICreateInstance"))
                .ok_or(NvencError::Library)?;
            (
                std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    ffi::GetMaxSupportedVersion,
                >(max_version),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::CreateInstance>(
                    create_instance,
                ),
            )
        };

        let mut version = 0;
        // SAFETY: writes one integer.
        self.check("NvEncodeAPIGetMaxSupportedVersion", unsafe {
            max_version(&raw mut version)
        })?;
        if version < ((ffi::API_MAJOR << 4) | ffi::API_MINOR) {
            return Err(NvencError::DriverTooOld(version >> 4, version & 0xF));
        }

        self.api.version = ffi::FUNCTION_LIST_VER;
        // SAFETY: the list is a properly versioned, writable `NV_ENCODE_API_FUNCTION_LIST`.
        let status = unsafe { create_instance(&raw mut *self.api) };
        self.check("NvEncodeAPICreateInstance", status)?;

        let mut params = ffi::OpenEncodeSessionExParams::zeroed();
        params.version = ffi::OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = ffi::DEVICE_TYPE_DIRECTX;
        params.device = device.as_raw();
        params.api_version = ffi::API_VERSION;
        let open = function(self.api.open_encode_session_ex, "nvEncOpenEncodeSessionEx")?;
        // SAFETY: the device outlives the session (the pipeline drops the encoder first), and
        // the session handle is written on success only.
        let status = unsafe { open(&raw mut params, &raw mut self.session) };
        if status == Status::SUCCESS {
            return Ok(());
        }
        let error = self.check("nvEncOpenEncodeSessionEx", status);
        // A failed open may still hand back a session, which must be destroyed.
        if !self.session.is_null()
            && let Some(destroy) = self.api.destroy_encoder
        {
            // SAFETY: destroys the half-open session the driver returned.
            let _ = unsafe { destroy(self.session) };
        }
        self.session = ptr::null_mut();
        error
    }

    fn require_codec(&self) -> Result<(), NvencError> {
        let count_guids = function(self.api.get_encode_guid_count, "nvEncGetEncodeGUIDCount")?;
        let list_guids = function(self.api.get_encode_guids, "nvEncGetEncodeGUIDs")?;
        let mut count = 0;
        // SAFETY: writes one integer for an open session.
        self.check("nvEncGetEncodeGUIDCount", unsafe {
            count_guids(self.session, &raw mut count)
        })?;
        let mut guids = vec![GUID::zeroed(); count as usize];
        let mut written = 0;
        // SAFETY: the array holds `count` GUIDs.
        self.check("nvEncGetEncodeGUIDs", unsafe {
            list_guids(self.session, guids.as_mut_ptr(), count, &raw mut written)
        })?;
        guids.truncate(written as usize);
        if guids.contains(&codec_guid(self.settings.codec)) {
            Ok(())
        } else {
            Err(NvencError::Unsupported(self.settings.codec))
        }
    }

    /// Fails when the frame is larger than the codec's encoder accepts (H.264 stops at 4096
    /// pixels across on every NVENC generation).
    fn require_size(&self) -> Result<(), NvencError> {
        let max_width = self.capability(ffi::CAPS_WIDTH_MAX)?;
        let max_height = self.capability(ffi::CAPS_HEIGHT_MAX)?;
        let Settings {
            codec,
            width,
            height,
            ..
        } = self.settings;
        if width > max_width || height > max_height {
            return Err(NvencError::TooLarge {
                codec,
                width,
                height,
                max_width,
                max_height,
            });
        }
        Ok(())
    }

    fn capability(&self, capability: u32) -> Result<u32, NvencError> {
        let get_caps = function(self.api.get_encode_caps, "nvEncGetEncodeCaps")?;
        let mut query = ffi::CapsParam::zeroed();
        query.version = ffi::CAPS_PARAM_VER;
        query.caps_to_query = capability;
        let mut value = 0;
        // SAFETY: a versioned query structure and one integer to write.
        self.check("nvEncGetEncodeCaps", unsafe {
            get_caps(
                self.session,
                codec_guid(self.settings.codec),
                &raw mut query,
                &raw mut value,
            )
        })?;
        Ok(value.unsigned_abs())
    }

    fn initialize(&mut self) -> Result<(), NvencError> {
        let settings = self.settings;
        let codec = codec_guid(settings.codec);

        let mut preset = ffi::PresetConfig::zeroed();
        preset.version = ffi::PRESET_CONFIG_VER;
        preset.config.version = ffi::CONFIG_VER;
        let get_preset = function(
            self.api.get_encode_preset_config_ex,
            "nvEncGetEncodePresetConfigEx",
        )?;
        // SAFETY: the preset structure is versioned and writable.
        self.check("nvEncGetEncodePresetConfigEx", unsafe {
            get_preset(
                self.session,
                codec,
                ffi::PRESET_P4,
                ffi::TUNING_HIGH_QUALITY,
                &raw mut preset,
            )
        })?;

        let mut config = preset.config;
        configure(&mut config, settings);

        let mut params = ffi::InitializeParams::zeroed();
        params.version = ffi::INITIALIZE_PARAMS_VER;
        params.encode_guid = codec;
        params.preset_guid = ffi::PRESET_P4;
        params.encode_width = settings.width;
        params.encode_height = settings.height;
        params.dar_width = settings.width;
        params.dar_height = settings.height;
        params.frame_rate_num = settings.frame_rate;
        params.frame_rate_den = 1;
        params.enable_encode_async = 1;
        params.enable_ptd = 1;
        params.encode_config = &raw mut config;
        params.max_encode_width = settings.width;
        params.max_encode_height = settings.height;
        params.tuning_info = ffi::TUNING_HIGH_QUALITY;
        let initialize = function(self.api.initialize_encoder, "nvEncInitializeEncoder")?;
        // SAFETY: `params` and the configuration it points to live across the call.
        self.check("nvEncInitializeEncoder", unsafe {
            initialize(self.session, &raw mut params)
        })
    }

    fn create_slot(&mut self, device: &ID3D11Device) -> Result<Slot, NvencError> {
        let description = D3D11_TEXTURE2D_DESC {
            Width: self.settings.width,
            Height: self.settings.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a plain texture description without initial data.
        unsafe { device.CreateTexture2D(&raw const description, None, Some(&raw mut texture)) }
            .map_err(|error| NvencError::Windows("create an encoder input texture", error))?;
        let texture = texture.ok_or(NvencError::Windows(
            "create an encoder input texture",
            windows::core::Error::empty(),
        ))?;

        // SAFETY: an unnamed auto-reset event with default security.
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| NvencError::Windows("create an encoder event", error))?;
        let mut slot = Slot {
            texture,
            registered: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            event,
            in_flight: None,
        };
        if let Err(error) = self.prepare_slot(&mut slot) {
            self.release_slot(&slot);
            return Err(error);
        }
        Ok(slot)
    }

    fn prepare_slot(&self, slot: &mut Slot) -> Result<(), NvencError> {
        let mut resource = ffi::RegisterResource::zeroed();
        resource.version = ffi::REGISTER_RESOURCE_VER;
        resource.resource_type = ffi::INPUT_RESOURCE_TYPE_DIRECTX;
        resource.width = self.settings.width;
        resource.height = self.settings.height;
        resource.resource_to_register = slot.texture.as_raw();
        resource.buffer_format = ffi::BUFFER_FORMAT_ARGB;
        resource.buffer_usage = ffi::INPUT_IMAGE;
        let register = function(self.api.register_resource, "nvEncRegisterResource")?;
        // SAFETY: the texture stays alive, and registered, as long as the slot.
        self.check("nvEncRegisterResource", unsafe {
            register(self.session, &raw mut resource)
        })?;
        slot.registered = resource.registered_resource;

        let mut buffer = ffi::CreateBitstreamBuffer::zeroed();
        buffer.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
        let create = function(
            self.api.create_bitstream_buffer,
            "nvEncCreateBitstreamBuffer",
        )?;
        // SAFETY: a versioned structure for an initialized session.
        self.check("nvEncCreateBitstreamBuffer", unsafe {
            create(self.session, &raw mut buffer)
        })?;
        slot.bitstream = buffer.bitstream_buffer;

        let mut event = ffi::EventParams::zeroed();
        event.version = ffi::EVENT_PARAMS_VER;
        event.completion_event = slot.event.0;
        let register_event = function(self.api.register_async_event, "nvEncRegisterAsyncEvent")?;
        // SAFETY: the event stays open until it is unregistered with the slot.
        self.check("nvEncRegisterAsyncEvent", unsafe {
            register_event(self.session, &raw mut event)
        })
    }

    /// The texture the next frame must be drawn into. When every slot is still in flight, waits
    /// for the oldest and hands its packet to `output` first.
    pub(super) fn input(
        &mut self,
        output: &mut impl FnMut(Encoded),
    ) -> Result<&ID3D11Texture2D, NvencError> {
        if self.slots[self.next].in_flight.is_some() {
            self.collect_one(true, output)?;
        }
        Ok(&self.slots[self.next].texture)
    }

    /// The texture holding the newest submitted picture, to repeat when the screen did not
    /// change.
    pub(super) fn latest(&self) -> Option<&ID3D11Texture2D> {
        self.latest.map(|slot| &self.slots[slot].texture)
    }

    /// Encodes the texture [`input`](Self::input) returned as frame `index`.
    pub(super) fn submit(&mut self, index: u64, fresh: bool) -> Result<(), NvencError> {
        let lock = self.device_lock.clone();
        let _locked = Locked::new(&lock);
        let slot = self.next;
        let mut map = ffi::MapInputResource::zeroed();
        map.version = ffi::MAP_INPUT_RESOURCE_VER;
        map.registered_resource = self.slots[slot].registered;
        let map_input = function(self.api.map_input_resource, "nvEncMapInputResource")?;
        // SAFETY: the resource is registered and not currently mapped.
        self.check("nvEncMapInputResource", unsafe {
            map_input(self.session, &raw mut map)
        })?;

        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.input_width = self.settings.width;
        picture.input_height = self.settings.height;
        picture.input_time_stamp = index;
        picture.input_buffer = map.mapped_resource;
        picture.output_bitstream = self.slots[slot].bitstream;
        picture.completion_event = self.slots[slot].event.0;
        picture.buffer_format = map.mapped_buffer_format;
        picture.picture_struct = ffi::PIC_STRUCT_FRAME;
        let encode = function(self.api.encode_picture, "nvEncEncodePicture")?;
        // SAFETY: the input is mapped and the output buffer is not locked or in flight.
        let status = unsafe { encode(self.session, &raw mut picture) };
        if let Err(error) = self.check("nvEncEncodePicture", status) {
            self.unmap(map.mapped_resource);
            return Err(error);
        }

        self.slots[slot].in_flight = Some(Frame {
            index,
            fresh,
            mapped: map.mapped_resource,
        });
        self.queue.push_back(slot);
        self.latest = Some(slot);
        self.next = (slot + 1) % SLOTS;
        Ok(())
    }

    /// Hands every frame NVENC has finished, oldest first, to `output` without waiting.
    pub(super) fn collect(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(false, output)? {}
        Ok(())
    }

    /// Waits for every frame in flight and hands them to `output`.
    pub(super) fn finish(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(true, output)? {}
        Ok(())
    }

    /// Collects the oldest frame in flight; reports whether there was one ready.
    fn collect_one(
        &mut self,
        wait: bool,
        output: &mut impl FnMut(Encoded),
    ) -> Result<bool, NvencError> {
        let Some(&slot) = self.queue.front() else {
            return Ok(false);
        };
        let timeout = if wait { ENCODE_TIMEOUT_MS } else { 0 };
        // SAFETY: the event belongs to the slot and is open.
        match unsafe { WaitForSingleObject(self.slots[slot].event, timeout) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT if !wait => return Ok(false),
            WAIT_TIMEOUT => {
                self.stalled = true;
                return Err(NvencError::Timeout);
            }
            _ => {
                return Err(NvencError::Windows(
                    "wait for NVENC",
                    windows::core::Error::from_thread(),
                ));
            }
        }
        self.queue.pop_front();
        let frame = self.slots[slot]
            .in_flight
            .take()
            .expect("a queued slot has a frame in flight");

        let result = {
            let _locked = Locked::new(&self.device_lock);
            let result = self.read_bitstream(slot);
            self.unmap(frame.mapped);
            result
        };
        let (picture_type, data) = result?;
        output(Encoded {
            index: frame.index,
            fresh: frame.fresh,
            picture_type,
            data,
        });
        Ok(true)
    }

    fn read_bitstream(&self, slot: usize) -> Result<(u32, Box<[u8]>), NvencError> {
        let mut lock = ffi::LockBitstream::zeroed();
        lock.version = ffi::LOCK_BITSTREAM_VER;
        lock.output_bitstream = self.slots[slot].bitstream;
        let lock_bitstream = function(self.api.lock_bitstream, "nvEncLockBitstream")?;
        let unlock_bitstream = function(self.api.unlock_bitstream, "nvEncUnlockBitstream")?;
        // SAFETY: the buffer's frame has completed; it stays locked only while it is copied.
        self.check("nvEncLockBitstream", unsafe {
            lock_bitstream(self.session, &raw mut lock)
        })?;
        // SAFETY: while locked, the driver guarantees `bitstream_size_in_bytes` readable bytes
        // at `bitstream_buffer_ptr`.
        let data: Box<[u8]> = unsafe {
            std::slice::from_raw_parts(
                lock.bitstream_buffer_ptr.cast::<u8>(),
                lock.bitstream_size_in_bytes as usize,
            )
        }
        .into();
        // SAFETY: unlocks the buffer locked above.
        self.check("nvEncUnlockBitstream", unsafe {
            unlock_bitstream(self.session, self.slots[slot].bitstream)
        })?;
        Ok((lock.picture_type, data))
    }

    fn unmap(&self, mapped: *mut std::ffi::c_void) {
        if let Some(unmap) = self.api.unmap_input_resource {
            // SAFETY: `mapped` came from a successful map that has not been undone.
            let _ = unsafe { unmap(self.session, mapped) };
        }
    }

    /// Ends the stream so the driver releases what it holds for it.
    fn end_of_stream(&self) {
        let (Some(encode), Some(slot)) = (self.api.encode_picture, self.slots.first()) else {
            return;
        };
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.encode_pic_flags = ffi::PIC_FLAG_EOS;
        picture.completion_event = slot.event.0;
        // SAFETY: an end-of-stream picture carries no buffers; its event is signalled once the
        // driver has flushed.
        if unsafe { encode(self.session, &raw mut picture) } == Status::SUCCESS {
            // SAFETY: the slot's event is open.
            let _ = unsafe { WaitForSingleObject(slot.event, ENCODE_TIMEOUT_MS) };
        }
    }

    fn release_slot(&self, slot: &Slot) {
        if let Some(frame) = &slot.in_flight {
            self.unmap(frame.mapped);
        }
        if let Some(unregister_event) = self.api.unregister_async_event {
            let mut event = ffi::EventParams::zeroed();
            event.version = ffi::EVENT_PARAMS_VER;
            event.completion_event = slot.event.0;
            // SAFETY: unregistering an event that is not registered is reported, not fatal.
            let _ = unsafe { unregister_event(self.session, &raw mut event) };
        }
        if !slot.bitstream.is_null()
            && let Some(destroy) = self.api.destroy_bitstream_buffer
        {
            // SAFETY: the buffer is unlocked and no longer in flight.
            let _ = unsafe { destroy(self.session, slot.bitstream) };
        }
        if !slot.registered.is_null()
            && let Some(unregister) = self.api.unregister_resource
        {
            // SAFETY: the resource is unmapped.
            let _ = unsafe { unregister(self.session, slot.registered) };
        }
        // SAFETY: the event is open and no longer registered with the driver.
        let _ = unsafe { CloseHandle(slot.event) };
    }

    fn check(&self, call: &'static str, status: Status) -> Result<(), NvencError> {
        if status == Status::SUCCESS {
            return Ok(());
        }
        let detail = self
            .api
            .get_last_error_string
            .filter(|_| !self.session.is_null())
            // SAFETY: returns a NUL-terminated string owned by the session, or null.
            .map(|last_error| unsafe { last_error(self.session) })
            .filter(|text| !text.is_null())
            // SAFETY: checked non-null above; copied out before the next call.
            .map(|text| {
                unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|text| !text.is_empty())
            .map(|text| format!(" ({text})"))
            .unwrap_or_default();
        Err(call_error(call, status, detail))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            // Frames still in flight are abandoned; their buffers are released below. A
            // stalled encoder is not waited for again.
            if !self.stalled {
                for &slot in &self.queue {
                    if let Some(event) = self.slots.get(slot).map(|slot| slot.event) {
                        // SAFETY: the slot's event is open.
                        let _ = unsafe { WaitForSingleObject(event, ENCODE_TIMEOUT_MS) };
                    }
                }
            }
            let _locked = Locked::new(&self.device_lock);
            if !self.stalled {
                self.end_of_stream();
            }
            for slot in &self.slots {
                self.release_slot(slot);
            }
            if let Some(destroy) = self.api.destroy_encoder {
                // SAFETY: every buffer and registration of the session is gone.
                let _ = unsafe { destroy(self.session) };
            }
        }
        // SAFETY: nothing from the library is used after this.
        let _ = unsafe { FreeLibrary(self.library) };
    }
}

fn load_library() -> Result<HMODULE, NvencError> {
    // The driver installs the library in System32; never look anywhere a planted copy could be.
    // SAFETY: loads a system library by name.
    unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| NvencError::Library)
}

fn function<T>(entry: Option<T>, call: &'static str) -> Result<T, NvencError> {
    entry.ok_or(NvencError::Call {
        call,
        status: "a missing entry point",
        detail: String::new(),
    })
}

fn call_error(call: &'static str, status: Status, detail: String) -> NvencError {
    if status == Status::INVALID_VERSION {
        return NvencError::VersionRejected;
    }
    NvencError::Call {
        call,
        status: status.name(),
        detail,
    }
}

const fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::CODEC_H264,
        Codec::Hevc => ffi::CODEC_HEVC,
        Codec::Av1 => ffi::CODEC_AV1,
    }
}

/// Adapts the preset's configuration: one keyframe per second, no B-frames, single-pass VBR
/// without lookahead (frames leave the encoder in order, one per input), 8-bit 4:2:0 with
/// BT.709 signalled, and the sequence header repeated on every keyframe so any of them can start
/// a clip.
fn configure(config: &mut ffi::Config, settings: Settings) {
    let gop = settings.frame_rate * GOP_SECONDS;
    config.gop_length = gop;
    config.frame_interval_p = 1;
    config.rc.rate_control_mode = ffi::RATE_CONTROL_VBR;
    config.rc.average_bit_rate = settings.bitrate;
    config.rc.max_bit_rate = settings.bitrate / 2 * 3;
    config.rc.flags &= !ffi::RC_ENABLE_LOOKAHEAD;
    config.rc.multi_pass = ffi::MULTI_PASS_DISABLED;

    // SAFETY: each arm writes the member of the union that belongs to the configured codec,
    // which the preset filled in.
    unsafe {
        match settings.codec {
            Codec::H264 => {
                config.profile_guid = ffi::PROFILE_H264_HIGH;
                let h264 = &mut config.codec.h264;
                h264.flags |= ffi::H264_REPEAT_SPS_PPS;
                h264.idr_period = gop;
                h264.chroma_format_idc = ffi::CHROMA_FORMAT_420;
                h264.vui.signal_bt709();
            }
            Codec::Hevc => {
                config.profile_guid = ffi::PROFILE_HEVC_MAIN;
                let hevc = &mut config.codec.hevc;
                hevc.flags &= !(ffi::HEVC_CHROMA_FORMAT_MASK | ffi::HEVC_BIT_DEPTH_MASK);
                hevc.flags |= ffi::HEVC_REPEAT_SPS_PPS
                    | (ffi::CHROMA_FORMAT_420 << ffi::HEVC_CHROMA_FORMAT_SHIFT);
                hevc.idr_period = gop;
                hevc.vui.signal_bt709();
            }
            Codec::Av1 => {
                config.profile_guid = ffi::PROFILE_AV1_MAIN;
                let av1 = &mut config.codec.av1;
                av1.flags &= !(ffi::AV1_ANNEX_B
                    | ffi::AV1_TIMING_INFO
                    | ffi::AV1_DECODER_MODEL_INFO
                    | ffi::AV1_FRAME_ID_NUMBERS
                    | ffi::AV1_DISABLE_SEQUENCE_HEADER
                    | ffi::AV1_CHROMA_FORMAT_MASK
                    | ffi::AV1_BIT_DEPTH_MASK);
                av1.flags |= ffi::AV1_REPEAT_SEQUENCE_HEADER
                    | (ffi::CHROMA_FORMAT_420 << ffi::AV1_CHROMA_FORMAT_SHIFT);
                av1.idr_period = gop;
                av1.color_primaries = ffi::BT709;
                av1.transfer_characteristics = ffi::BT709;
                av1.matrix_coefficients = ffi::BT709;
                av1.color_range = 0;
            }
        }
    }
}
