//! Hardware encoding through NVENC (`nvEncodeAPI64.dll`): an asynchronous ring of slots fed straight from video memory.

use crate::{
    config::Codec,
    gpu::{
        capture::{EncoderDevice, Surface},
        cuda::{DeviceBuffer, DevicePointer},
        cuda_output::{CudaRing, StagingId, StagingLayout},
        device::{Locked, null_output},
        ffi::{Library, Symbol, Zeroed},
        pacer::FrameIndex,
    },
    replay::{Matrix, Picture, Slot as StoreSlot},
};
use std::{
    collections::VecDeque,
    ffi::{CStr, c_void},
    ptr,
};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::{
            Direct3D9::IDirect3DSurface9,
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
                ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::{GUID, Interface, Owned, s, w},
};

#[cfg(test)]
mod d3d12_cost;
mod ffi;

pub(super) use ffi::Status;

/// Two suffice while the GPU keeps up; the third absorbs a frame NVENC finishes late.
const SLOTS: usize = 3;
const ENCODE_TIMEOUT_MS: u32 = 2000;
/// Two keyframes a second: a clip starts within half a second of where it is asked to, and
/// the rest of the bitrate goes to the P-frames. At 60 fps that is Instant Replay's 30 frames.
const KEYFRAMES_PER_SECOND: u32 = 2;
/// Bitstream bytes read back with a frame's header, enough to tell a keyframe.
const HEAD_BYTES: usize = 16 << 10;

/// How NVENC lays a frame out in a video-memory output buffer.
pub(super) const STAGING_LAYOUT: StagingLayout = StagingLayout {
    header_bytes: ffi::ENCODE_OUT_PARAMS_BYTES,
    size_offset: ffi::ENCODE_OUT_PARAMS_SIZE_OFFSET,
    head_bytes: HEAD_BYTES,
};

#[derive(Debug, Error)]
pub(super) enum NvencError {
    #[error("nvEncodeAPI64.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error(
        "the NVIDIA driver supports NVENC API {0}.{1}; update the driver to use API {major}.{minor} or newer",
        major = ffi::API_MAJOR,
        minor = ffi::API_MINOR,
    )]
    DriverTooOld(u32, u32),
    #[error(
        "the NVIDIA driver rejected the NVENC API {major}.{minor} structures; update the driver",
        major = ffi::API_MAJOR,
        minor = ffi::API_MINOR,
    )]
    VersionRejected,
    #[error("this GPU's NVENC has no {0} encoder")]
    Unsupported(Codec),
    #[error(
        "the NVIDIA driver reports {value} for NVENC capability {capability}, which is not a limit"
    )]
    BadCapability { capability: u32, value: i32 },
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
        status: Status,
        detail: String,
    },
    #[error("the NVIDIA driver does not provide {0}")]
    MissingEntryPoint(&'static str),
    #[error("NVENC did not finish a frame within {ENCODE_TIMEOUT_MS} ms")]
    Timeout,
    #[error("{0}: {1}")]
    Windows(&'static str, windows::core::Error),
    #[error("{0}: {1}")]
    Cuda(&'static str, super::cuda::CudaError),
    #[error("move a frame into the replay buffer in GPU memory: {0}")]
    Ring(crate::replay::StoreError),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    pub(super) codec: Codec,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) frame_rate: u32,
    /// Bits per second.
    pub(super) bitrate_bps: u32,
    pub(super) matrix: Matrix,
}

pub(super) enum Output {
    Memory,
    /// Written into video memory and moved into the ring there; CUDA sessions only.
    VideoMemory(CudaRing),
}

pub(super) struct Encoded {
    pub(super) index: FrameIndex,
    pub(super) picture: Picture,
    /// Reported only for a frame locked in system memory.
    picture_type: Option<u32>,
    pub(super) data: Payload,
}

pub(super) enum Payload {
    Memory(Box<[u8]>),
    Stored { slot: StoreSlot, head: Box<[u8]> },
}

impl Encoded {
    pub(super) fn is_idr(&self) -> bool {
        self.picture_type == Some(ffi::PIC_TYPE_IDR)
    }

    pub(super) fn head(&self) -> &[u8] {
        match &self.data {
            Payload::Memory(data) => data,
            Payload::Stored { head, .. } => head,
        }
    }
}

struct VideoOutput {
    staging: StagingId,
    registered: ffi::Registered,
    mapped: ffi::Mapped,
}

pub(super) struct Encoder {
    settings: Settings,
    slots: Vec<Slot>,
    surfaces: Vec<Option<RegisteredSurface>>,
    queue: VecDeque<usize>,
    next: usize,
    latest: Option<Input>,
    /// The capture frame pool uses the device from its own threads.
    device_lock: ID3D11Multithread,
    stalled: bool,
    ring: Option<CudaRing>,
    /// Declared last: everything registered with it goes first.
    session: Session,
}

/// An open encode session and the library it lives in; `Drop` destroys it.
struct Session {
    api: Box<ffi::FunctionList>,
    handle: ffi::Handle,
    /// Declared last so it is freed last.
    _library: Library,
}

struct Frame {
    index: FrameIndex,
    picture: Picture,
    input: Input,
    mapped: ffi::Mapped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    Slot(usize),
    Surface(usize),
}

struct RegisteredSurface {
    handle: Surface,
    registered: ffi::Registered,
}

struct Slot {
    texture: Option<ID3D11Texture2D>,
    registered: ffi::Registered,
    output: SlotOutput,
    event: Owned<HANDLE>,
    in_flight: Option<Frame>,
}

enum SlotOutput {
    /// A buffer of the driver's, locked to read.
    Bitstream(ffi::Bitstream),
    VideoMemory(VideoOutput),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Poll,
    Block,
}

impl Encoder {
    pub(super) fn open(
        device: &ID3D11Device,
        on: &EncoderDevice,
        settings: Settings,
        output: Output,
    ) -> Result<Self, NvencError> {
        let device_lock: ID3D11Multithread = device
            .cast()
            .map_err(|error| NvencError::Windows("lock the device", error))?;
        let ring = match output {
            Output::Memory => None,
            Output::VideoMemory(ring) => {
                debug_assert!(
                    matches!(on, EncoderDevice::Cuda { .. }),
                    "frames go to video memory on a CUDA session only"
                );
                Some(ring)
            }
        };
        let mut encoder = Self {
            settings,
            slots: Vec::with_capacity(SLOTS),
            surfaces: Vec::new(),
            queue: VecDeque::with_capacity(SLOTS),
            next: 0,
            latest: None,
            device_lock,
            stalled: false,
            ring,
            session: Session::open(device, on)?,
        };
        // From here on, `Drop` releases whatever has been created when a step fails.
        encoder.require_codec()?;
        encoder.require_size()?;
        encoder.initialize()?;
        for _ in 0..SLOTS {
            let slot = encoder.create_slot(device, on)?;
            encoder.slots.push(slot);
        }
        Ok(encoder)
    }

    pub(super) const fn settings(&self) -> Settings {
        self.settings
    }

    fn require_codec(&self) -> Result<(), NvencError> {
        if self
            .session
            .encode_guids()?
            .contains(&codec_guid(self.settings.codec))
        {
            Ok(())
        } else {
            Err(NvencError::Unsupported(self.settings.codec))
        }
    }

    fn require_size(&self) -> Result<(), NvencError> {
        let Settings {
            codec,
            width,
            height,
            ..
        } = self.settings;
        let max_width = self.session.capability(codec, ffi::CAPS_WIDTH_MAX)?;
        let max_height = self.session.capability(codec, ffi::CAPS_HEIGHT_MAX)?;
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

    fn initialize(&self) -> Result<(), NvencError> {
        let mut config = self.preset_config()?;
        configure(&mut config, self.settings);
        let mut params = initialize_params(self.settings, &mut config);
        if self.ring.is_some() {
            params.flags |= ffi::INITIALIZE_OUTPUT_IN_VIDMEM;
        }
        let initialize = entry_point(
            self.session.api.initialize_encoder,
            "nvEncInitializeEncoder",
        )?;
        // SAFETY: `params` and the configuration it points to live across the call.
        self.check("nvEncInitializeEncoder", unsafe {
            initialize(self.session.handle, &raw mut params)
        })
    }

    fn preset_config(&self) -> Result<ffi::Config, NvencError> {
        let mut preset = ffi::PresetConfig::zeroed();
        preset.version = ffi::PRESET_CONFIG_VER;
        preset.config.version = ffi::CONFIG_VER;
        let get_preset = entry_point(
            self.session.api.get_encode_preset_config_ex,
            "nvEncGetEncodePresetConfigEx",
        )?;
        // SAFETY: the preset structure is versioned and writable.
        self.check("nvEncGetEncodePresetConfigEx", unsafe {
            get_preset(
                self.session.handle,
                codec_guid(self.settings.codec),
                ffi::PRESET_P4,
                ffi::TUNING_HIGH_QUALITY,
                &raw mut preset,
            )
        })?;
        Ok(preset.config)
    }

    fn create_slot(
        &mut self,
        device: &ID3D11Device,
        on: &EncoderDevice,
    ) -> Result<Slot, NvencError> {
        let texture = match on {
            EncoderDevice::Direct3D => Some(create_input_texture(device, self.settings)?),
            EncoderDevice::Cuda { .. } | EncoderDevice::Direct3D9(_) => None,
        };
        let event = create_event()?;
        let mut slot = Slot {
            texture,
            registered: ptr::null_mut(),
            output: self.create_output()?,
            event,
            in_flight: None,
        };
        if let Err(error) = self.prepare_slot(&mut slot) {
            self.release_slot(&slot);
            return Err(error);
        }
        Ok(slot)
    }

    fn create_output(&mut self) -> Result<SlotOutput, NvencError> {
        let Some(ring) = self.ring.as_mut() else {
            return Ok(SlotOutput::Bitstream(self.create_bitstream()?));
        };
        let (staging, pointer) = ring
            .allocate_staging(staging_bytes(self.settings))
            .map_err(|error| NvencError::Cuda("allocate an output buffer", error))?;
        Ok(SlotOutput::VideoMemory(
            self.register_output(staging, pointer)?,
        ))
    }

    fn prepare_slot(&self, slot: &mut Slot) -> Result<(), NvencError> {
        if let Some(texture) = &slot.texture {
            slot.registered = self.register_texture(texture)?;
        }
        self.register_event(&slot.event)
    }

    fn register_output(
        &self,
        staging: StagingId,
        pointer: DevicePointer,
    ) -> Result<VideoOutput, NvencError> {
        let bytes = staging_bytes(self.settings);
        let mut params = ffi::RegisterResource::zeroed();
        params.version = ffi::REGISTER_RESOURCE_VER;
        params.resource_type = ffi::INPUT_RESOURCE_TYPE_CUDA_DEVICE_POINTER;
        // A buffer of bytes is registered as one row of them, as NVIDIA's sample does.
        params.width = u32::try_from(bytes).expect("an output buffer fits 32 bits");
        params.height = 1;
        params.pitch = params.width;
        params.resource_to_register = pointer.as_api_pointer();
        params.buffer_format = ffi::BUFFER_FORMAT_U8;
        params.buffer_usage = ffi::OUTPUT_BITSTREAM;
        let register = entry_point(self.session.api.register_resource, "nvEncRegisterResource")?;
        // SAFETY: the buffer stays allocated and registered as long as its slot.
        self.check("nvEncRegisterResource", unsafe {
            register(self.session.handle, &raw mut params)
        })?;
        let registered = params.registered_resource;
        // Mapped once: one frame at a time writes the buffer.
        match self.map_input(registered) {
            Ok(map) => Ok(VideoOutput {
                staging,
                registered,
                mapped: map.mapped_resource,
            }),
            Err(error) => {
                if let Some(unregister) = self.session.api.unregister_resource {
                    // SAFETY: the resource is registered and not mapped.
                    let _ = unsafe { unregister(self.session.handle, registered) };
                }
                Err(error)
            }
        }
    }

    fn register_texture(&self, texture: &ID3D11Texture2D) -> Result<ffi::Registered, NvencError> {
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_DIRECTX,
            texture.as_raw(),
            0,
            ffi::BUFFER_FORMAT_ARGB,
        )
    }

    fn register_d3d9_surface(
        &self,
        surface: &IDirect3DSurface9,
    ) -> Result<ffi::Registered, NvencError> {
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_DIRECTX,
            surface.as_raw(),
            0,
            ffi::BUFFER_FORMAT_ARGB,
        )
    }

    fn register_cuda_buffer(
        &self,
        buffer: &DeviceBuffer,
        pitch: u32,
    ) -> Result<ffi::Registered, NvencError> {
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_CUDA_DEVICE_POINTER,
            buffer.pointer().as_api_pointer(),
            pitch,
            ffi::BUFFER_FORMAT_ARGB,
        )
    }

    fn register_resource(
        &self,
        resource_type: u32,
        resource: *mut c_void,
        pitch: u32,
        buffer_format: u32,
    ) -> Result<ffi::Registered, NvencError> {
        let mut params = ffi::RegisterResource::zeroed();
        params.version = ffi::REGISTER_RESOURCE_VER;
        params.resource_type = resource_type;
        params.width = self.settings.width;
        params.height = self.settings.height;
        params.pitch = pitch;
        params.resource_to_register = resource;
        params.buffer_format = buffer_format;
        params.buffer_usage = ffi::INPUT_IMAGE;
        let register = entry_point(self.session.api.register_resource, "nvEncRegisterResource")?;
        // SAFETY: the resource stays alive and registered as long as its slot or surface.
        self.check("nvEncRegisterResource", unsafe {
            register(self.session.handle, &raw mut params)
        })?;
        Ok(params.registered_resource)
    }

    fn create_bitstream(&self) -> Result<ffi::Bitstream, NvencError> {
        let mut buffer = ffi::CreateBitstreamBuffer::zeroed();
        buffer.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
        let create = entry_point(
            self.session.api.create_bitstream_buffer,
            "nvEncCreateBitstreamBuffer",
        )?;
        // SAFETY: a versioned structure for an initialized session.
        self.check("nvEncCreateBitstreamBuffer", unsafe {
            create(self.session.handle, &raw mut buffer)
        })?;
        Ok(buffer.bitstream_buffer)
    }

    fn register_event(&self, event: &Owned<HANDLE>) -> Result<(), NvencError> {
        let mut params = ffi::EventParams::zeroed();
        params.version = ffi::EVENT_PARAMS_VER;
        params.completion_event = event.0;
        let register_event = entry_point(
            self.session.api.register_async_event,
            "nvEncRegisterAsyncEvent",
        )?;
        // SAFETY: the event stays open until it is unregistered with the slot.
        self.check("nvEncRegisterAsyncEvent", unsafe {
            register_event(self.session.handle, &raw mut params)
        })
    }

    /// Frees the slot the next frame goes into, collecting older frames as needed.
    pub(super) fn reserve(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.slots[self.next].in_flight.is_some() || self.reads(Input::Slot(self.next)) {
            self.collect_one(Wait::Block, output)?;
        }
        Ok(())
    }

    pub(super) fn input(&self) -> Option<&ID3D11Texture2D> {
        self.slots[self.next].texture.as_ref()
    }

    pub(super) const fn has_latest(&self) -> bool {
        self.latest.is_some()
    }

    pub(super) fn latest_texture(&self) -> Option<&ID3D11Texture2D> {
        match self.latest? {
            Input::Slot(slot) => self.slots[slot].texture.as_ref(),
            Input::Surface(surface) => match &self.registered_surface(surface).handle {
                Surface::Texture(texture) => Some(texture),
                Surface::CudaBuffer { .. } | Surface::Direct3D9(_) => None,
            },
        }
    }

    fn registered_surface(&self, surface: usize) -> &RegisteredSurface {
        self.surfaces[surface]
            .as_ref()
            .expect("only registered surfaces are submitted")
    }

    pub(super) fn has_surface(&self, surface: usize) -> bool {
        self.surfaces.get(surface).is_some_and(Option::is_some)
    }

    pub(super) fn register_surface(
        &mut self,
        surface: usize,
        handle: &Surface,
    ) -> Result<(), NvencError> {
        debug_assert!(
            !self.has_surface(surface),
            "a surface is registered once; registering it again would leak the first handle"
        );
        let registered = match handle {
            Surface::Texture(texture) => self.register_texture(texture)?,
            Surface::CudaBuffer { buffer, pitch } => self.register_cuda_buffer(buffer, *pitch)?,
            Surface::Direct3D9(surface) => self.register_d3d9_surface(surface)?,
        };
        if self.surfaces.len() <= surface {
            self.surfaces.resize_with(surface + 1, || None);
        }
        self.surfaces[surface] = Some(RegisteredSurface {
            handle: handle.clone(),
            registered,
        });
        Ok(())
    }

    pub(super) fn holds_surface(&self, surface: usize) -> bool {
        self.latest == Some(Input::Surface(surface)) || self.reads(Input::Surface(surface))
    }

    /// Waits until NVENC is done reading `surface`, so the capture may draw into it again.
    pub(super) fn release_surface(
        &mut self,
        surface: usize,
        output: &mut impl FnMut(Encoded),
    ) -> Result<(), NvencError> {
        while self.reads(Input::Surface(surface)) {
            self.collect_one(Wait::Block, output)?;
        }
        Ok(())
    }

    fn reads(&self, input: Input) -> bool {
        self.queue.iter().any(|&slot| {
            self.slots[slot]
                .in_flight
                .as_ref()
                .is_some_and(|frame| frame.input == input)
        })
    }

    pub(super) fn submit(&mut self, index: FrameIndex, picture: Picture) -> Result<(), NvencError> {
        self.submit_input(index, picture, Input::Slot(self.next))
    }

    pub(super) fn submit_surface(
        &mut self,
        index: FrameIndex,
        surface: usize,
    ) -> Result<(), NvencError> {
        self.submit_input(index, Picture::Fresh, Input::Surface(surface))
    }

    /// False while NVENC is still reading the picture for the frame before: a texture is mapped for one frame at a time.
    pub(super) fn repeat(&mut self, index: FrameIndex) -> Result<bool, NvencError> {
        let Some(latest) = self.latest else {
            return Ok(false);
        };
        if self.reads(latest) {
            return Ok(false);
        }
        self.submit_input(index, Picture::Repeated, latest)?;
        Ok(true)
    }

    pub(super) fn repeat_once_free(
        &mut self,
        index: FrameIndex,
        output: &mut impl FnMut(Encoded),
    ) -> Result<(), NvencError> {
        let latest = self
            .latest
            .expect("a repeat is asked for only once a picture has been submitted");
        while self.reads(latest) {
            self.collect_one(Wait::Block, output)?;
        }
        self.submit_input(index, Picture::Repeated, latest)
    }

    fn submit_input(
        &mut self,
        index: FrameIndex,
        picture: Picture,
        input: Input,
    ) -> Result<(), NvencError> {
        let _locked = Locked::new(&self.device_lock);
        let slot = self.next;
        let registered = match input {
            Input::Slot(slot) => self.slots[slot].registered,
            Input::Surface(surface) => self.registered_surface(surface).registered,
        };
        let map = self.map_input(registered)?;
        self.encode_picture(slot, index, &map)?;
        self.slots[slot].in_flight = Some(Frame {
            index,
            picture,
            input,
            mapped: map.mapped_resource,
        });
        self.queue.push_back(slot);
        self.latest = Some(input);
        self.next = (slot + 1) % SLOTS;
        Ok(())
    }

    fn map_input(&self, registered: ffi::Registered) -> Result<ffi::MapInputResource, NvencError> {
        let mut map = ffi::MapInputResource::zeroed();
        map.version = ffi::MAP_INPUT_RESOURCE_VER;
        map.registered_resource = registered;
        let map_input = entry_point(self.session.api.map_input_resource, "nvEncMapInputResource")?;
        // SAFETY: the resource is registered and not mapped; `reserve`, `repeat` and `release_surface` see to that.
        self.check("nvEncMapInputResource", unsafe {
            map_input(self.session.handle, &raw mut map)
        })?;
        Ok(map)
    }

    fn encode_picture(
        &self,
        slot: usize,
        index: FrameIndex,
        map: &ffi::MapInputResource,
    ) -> Result<(), NvencError> {
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.input_width = self.settings.width;
        picture.input_height = self.settings.height;
        picture.input_time_stamp = index.get();
        picture.input_buffer = map.mapped_resource;
        picture.output_bitstream = match &self.slots[slot].output {
            SlotOutput::Bitstream(bitstream) => *bitstream,
            SlotOutput::VideoMemory(output) => output.mapped,
        };
        picture.completion_event = self.slots[slot].event.0;
        picture.buffer_format = map.mapped_buffer_format;
        picture.picture_struct = ffi::PIC_STRUCT_FRAME;
        let queued =
            entry_point(self.session.api.encode_picture, "nvEncEncodePicture").and_then(|encode| {
                // SAFETY: the input is mapped and the output buffer is not locked or in flight.
                let status = unsafe { encode(self.session.handle, &raw mut picture) };
                self.check("nvEncEncodePicture", status)
            });
        if let Err(error) = queued {
            self.unmap(map.mapped_resource);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn collect(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(Wait::Poll, output)? {}
        Ok(())
    }

    pub(super) fn finish(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(Wait::Block, output)? {}
        Ok(())
    }

    fn collect_one(
        &mut self,
        wait: Wait,
        output: &mut impl FnMut(Encoded),
    ) -> Result<bool, NvencError> {
        let Some(&slot) = self.queue.front() else {
            return Ok(false);
        };
        if !self.await_completion(slot, wait)? {
            return Ok(false);
        }
        self.queue.pop_front();
        let frame = self.slots[slot]
            .in_flight
            .take()
            .expect("a queued slot has a frame in flight");

        // The device lock is let go before `output` runs, which takes the replay buffer's lock.
        let result = {
            let _locked = Locked::new(&self.device_lock);
            let result = match &self.slots[slot].output {
                SlotOutput::Bitstream(bitstream) => self.read_bitstream(*bitstream),
                SlotOutput::VideoMemory(output) => {
                    take_from_video_memory(self.ring.as_mut(), output.staging)
                }
            };
            self.unmap(frame.mapped);
            result
        };
        let (picture_type, data) = result?;
        output(Encoded {
            index: frame.index,
            picture: frame.picture,
            picture_type,
            data,
        });
        Ok(true)
    }

    fn await_completion(&mut self, slot: usize, wait: Wait) -> Result<bool, NvencError> {
        let timeout = match wait {
            Wait::Poll => 0,
            Wait::Block => ENCODE_TIMEOUT_MS,
        };
        // SAFETY: the event belongs to the slot and is open.
        match unsafe { WaitForSingleObject(*self.slots[slot].event, timeout) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT if wait == Wait::Poll => Ok(false),
            WAIT_TIMEOUT => {
                self.stalled = true;
                Err(NvencError::Timeout)
            }
            _ => Err(NvencError::Windows(
                "wait for NVENC",
                windows::core::Error::from_thread(),
            )),
        }
    }

    fn read_bitstream(
        &self,
        bitstream: ffi::Bitstream,
    ) -> Result<(Option<u32>, Payload), NvencError> {
        let mut lock = ffi::LockBitstream::zeroed();
        lock.version = ffi::LOCK_BITSTREAM_VER;
        lock.output_bitstream = bitstream;
        // The event has fired; a lock that could wait still pays for it.
        lock.flags = ffi::LOCK_DO_NOT_WAIT;
        let lock_bitstream = entry_point(self.session.api.lock_bitstream, "nvEncLockBitstream")?;
        let unlock_bitstream =
            entry_point(self.session.api.unlock_bitstream, "nvEncUnlockBitstream")?;
        // SAFETY: the buffer's frame has completed; it stays locked only while it is copied.
        self.check("nvEncLockBitstream", unsafe {
            lock_bitstream(self.session.handle, &raw mut lock)
        })?;
        let data: Box<[u8]> = if lock.bitstream_buffer_ptr.is_null() {
            Box::default()
        } else {
            // SAFETY: while locked, `bitstream_size_in_bytes` bytes are readable at the non-null pointer.
            unsafe {
                std::slice::from_raw_parts(
                    lock.bitstream_buffer_ptr.cast::<u8>(),
                    lock.bitstream_size_in_bytes as usize,
                )
            }
            .into()
        };
        // SAFETY: unlocks the buffer locked above; `data` no longer points into it.
        self.check("nvEncUnlockBitstream", unsafe {
            unlock_bitstream(self.session.handle, bitstream)
        })?;
        Ok((Some(lock.picture_type), Payload::Memory(data)))
    }

    fn unmap(&self, mapped: ffi::Mapped) {
        if let Some(unmap) = self.session.api.unmap_input_resource {
            // SAFETY: `mapped` came from a successful map that has not been undone.
            let _ = unsafe { unmap(self.session.handle, mapped) };
        }
    }

    fn check(&self, call: &'static str, status: Status) -> Result<(), NvencError> {
        self.session.check(call, status)
    }

    fn await_in_flight(&self) {
        let events = self
            .queue
            .iter()
            .filter_map(|&slot| self.slots.get(slot))
            .map(|slot| *slot.event);
        for event in events {
            // SAFETY: the slot's event is open.
            let _ = unsafe { WaitForSingleObject(event, ENCODE_TIMEOUT_MS) };
        }
    }

    fn end_of_stream(&self) {
        let (Some(encode), Some(slot)) = (self.session.api.encode_picture, self.slots.first())
        else {
            return;
        };
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.encode_pic_flags = ffi::PIC_FLAG_EOS;
        picture.completion_event = slot.event.0;
        // SAFETY: an end-of-stream picture carries no buffers.
        if unsafe { encode(self.session.handle, &raw mut picture) } == Status::SUCCESS {
            // SAFETY: the slot's event is open.
            let _ = unsafe { WaitForSingleObject(*slot.event, ENCODE_TIMEOUT_MS) };
        }
    }

    fn unregister_surfaces(&self) {
        let Some(unregister) = self.session.api.unregister_resource else {
            return;
        };
        for surface in self.surfaces.iter().flatten() {
            // SAFETY: the resource is registered and unmapped.
            let _ = unsafe { unregister(self.session.handle, surface.registered) };
        }
    }

    fn release_slot(&self, slot: &Slot) {
        if let Some(frame) = &slot.in_flight {
            self.unmap(frame.mapped);
        }
        if let Some(unregister_event) = self.session.api.unregister_async_event {
            let mut params = ffi::EventParams::zeroed();
            params.version = ffi::EVENT_PARAMS_VER;
            params.completion_event = slot.event.0;
            // SAFETY: unregistering an event that is not registered is reported, not fatal.
            let _ = unsafe { unregister_event(self.session.handle, &raw mut params) };
        }
        match &slot.output {
            SlotOutput::Bitstream(bitstream) => {
                if let Some(destroy) = self.session.api.destroy_bitstream_buffer {
                    // SAFETY: the buffer is unlocked and no longer in flight.
                    let _ = unsafe { destroy(self.session.handle, *bitstream) };
                }
            }
            SlotOutput::VideoMemory(output) => {
                self.unmap(output.mapped);
                if let Some(unregister) = self.session.api.unregister_resource {
                    // SAFETY: unmapped and no longer in flight; the ring frees it afterwards.
                    let _ = unsafe { unregister(self.session.handle, output.registered) };
                }
            }
        }
        if !slot.registered.is_null()
            && let Some(unregister) = self.session.api.unregister_resource
        {
            // SAFETY: the resource is unmapped.
            let _ = unsafe { unregister(self.session.handle, slot.registered) };
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.stalled {
            self.await_in_flight();
        }
        let _locked = Locked::new(&self.device_lock);
        if !self.stalled {
            self.end_of_stream();
        }
        for slot in &self.slots {
            self.release_slot(slot);
        }
        self.unregister_surfaces();
        if self.stalled
            && let Some(ring) = &mut self.ring
        {
            ring.leak_staging();
        }
        // Under the device lock, which `Drop` alone would not hold.
        self.session.destroy();
    }
}

impl Session {
    fn open(device: &ID3D11Device, on: &EncoderDevice) -> Result<Self, NvencError> {
        let (device_type, device) = match on {
            EncoderDevice::Direct3D => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
            EncoderDevice::Cuda { context, .. } => (ffi::DEVICE_TYPE_CUDA, context.as_ptr()),
            EncoderDevice::Direct3D9(device) => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
        };
        Self::open_on(device_type, device)
    }

    /// `device` must outlive the session.
    fn open_on(device_type: u32, device: *mut c_void) -> Result<Self, NvencError> {
        let library = load_library()?;
        let api = create_api(&library)?;
        let mut params = ffi::OpenEncodeSessionExParams::zeroed();
        params.version = ffi::OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = device_type;
        params.device = device;
        params.api_version = ffi::API_VERSION;
        let open = entry_point(api.open_encode_session_ex, "nvEncOpenEncodeSessionEx")?;
        let mut session = Self {
            api,
            handle: ptr::null_mut(),
            _library: library,
        };
        // SAFETY: the device outlives the session. A failed open may still hand back a
        // handle, which `Drop` destroys.
        let status = unsafe { open(&raw mut params, &raw mut session.handle) };
        session.check("nvEncOpenEncodeSessionEx", status)?;
        Ok(session)
    }

    fn encode_guids(&self) -> Result<Vec<GUID>, NvencError> {
        let count_guids = entry_point(self.api.get_encode_guid_count, "nvEncGetEncodeGUIDCount")?;
        let list_guids = entry_point(self.api.get_encode_guids, "nvEncGetEncodeGUIDs")?;
        let mut count = 0;
        // SAFETY: writes one integer for an open session.
        self.check("nvEncGetEncodeGUIDCount", unsafe {
            count_guids(self.handle, &raw mut count)
        })?;
        let mut guids = vec![GUID::zeroed(); count as usize];
        let mut written = 0;
        // SAFETY: the array holds `count` GUIDs.
        self.check("nvEncGetEncodeGUIDs", unsafe {
            list_guids(self.handle, guids.as_mut_ptr(), count, &raw mut written)
        })?;
        guids.truncate(written as usize);
        Ok(guids)
    }

    fn capability(&self, codec: Codec, capability: u32) -> Result<u32, NvencError> {
        let mut query = ffi::CapsParam::zeroed();
        query.version = ffi::CAPS_PARAM_VER;
        query.caps_to_query = capability;
        let get_caps = entry_point(self.api.get_encode_caps, "nvEncGetEncodeCaps")?;
        let mut value = 0;
        // SAFETY: a versioned query structure and one integer to write.
        self.check("nvEncGetEncodeCaps", unsafe {
            get_caps(
                self.handle,
                codec_guid(codec),
                &raw mut query,
                &raw mut value,
            )
        })?;
        u32::try_from(value).map_err(|_| NvencError::BadCapability { capability, value })
    }

    fn check(&self, call: &'static str, status: Status) -> Result<(), NvencError> {
        check(call, status, || self.last_error_detail())
    }

    fn last_error_detail(&self) -> String {
        let Some(last_error) = self.api.get_last_error_string else {
            return String::new();
        };
        if self.handle.is_null() {
            return String::new();
        }
        // SAFETY: returns a NUL-terminated string owned by the session, or null.
        let text = unsafe { last_error(self.handle) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: not null; copied out before the next call into the session.
        let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
        if text.is_empty() {
            return String::new();
        }
        format!(" ({text})")
    }

    /// Every buffer and registration of the session must be gone.
    fn destroy(&mut self) {
        if self.handle.is_null() {
            return;
        }
        if let Some(destroy) = self.api.destroy_encoder {
            // SAFETY: a live handle, used by nothing afterwards.
            let _ = unsafe { destroy(self.handle) };
        }
        self.handle = ptr::null_mut();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn take_from_video_memory(
    ring: Option<&mut CudaRing>,
    staging: StagingId,
) -> Result<(Option<u32>, Payload), NvencError> {
    let ring = ring.expect("a slot has an output buffer in video memory only with a ring");
    let (slot, head) = ring.take(staging).map_err(NvencError::Ring)?;
    Ok((None, Payload::Stored { slot, head }))
}

fn load_library() -> Result<Library, NvencError> {
    Library::load(w!("nvEncodeAPI64.dll")).map_err(|_| NvencError::Library)
}

fn create_api(library: &Library) -> Result<Box<ffi::FunctionList>, NvencError> {
    let (max_version, create_instance) = resolve_exports(library)?;
    require_driver_version(*max_version)?;
    let mut api = Box::new(ffi::FunctionList::zeroed());
    api.version = ffi::FUNCTION_LIST_VER;
    // SAFETY: the list is a properly versioned, writable `NV_ENCODE_API_FUNCTION_LIST`.
    let status = unsafe { create_instance(&raw mut *api) };
    check("NvEncodeAPICreateInstance", status, String::new)?;
    Ok(api)
}

fn require_driver_version(max_version: ffi::GetMaxSupportedVersion) -> Result<(), NvencError> {
    let mut version = 0;
    // SAFETY: writes one integer.
    let status = unsafe { max_version(&raw mut version) };
    check("NvEncodeAPIGetMaxSupportedVersion", status, String::new)?;
    if version < ffi::REQUIRED_MAX_VERSION {
        let (major, minor) = ffi::split_max_version(version);
        return Err(NvencError::DriverTooOld(major, minor));
    }
    Ok(())
}

fn resolve_exports(
    library: &Library,
) -> Result<
    (
        Symbol<'_, ffi::GetMaxSupportedVersion>,
        Symbol<'_, ffi::CreateInstance>,
    ),
    NvencError,
> {
    // SAFETY: declared `NVENCSTATUS NVENCAPI (uint32_t*)` in nvEncodeAPI.h.
    let max_version = unsafe { library.export(s!("NvEncodeAPIGetMaxSupportedVersion")) }
        .ok_or(NvencError::Library)?;
    // SAFETY: declared `NVENCSTATUS NVENCAPI (NV_ENCODE_API_FUNCTION_LIST*)` in nvEncodeAPI.h.
    let create_instance =
        unsafe { library.export(s!("NvEncodeAPICreateInstance")) }.ok_or(NvencError::Library)?;
    Ok((max_version, create_instance))
}

fn entry_point<T>(entry: Option<T>, call: &'static str) -> Result<T, NvencError> {
    entry.ok_or(NvencError::MissingEntryPoint(call))
}

/// The codecs this GPU's NVENC has an encoder for, in `Codec::ALL` order.
pub(super) fn supported_codecs(device: &ID3D11Device) -> Result<Vec<Codec>, NvencError> {
    let session = Session::open(device, &EncoderDevice::Direct3D)?;
    Ok(codecs_among(&session.encode_guids()?))
}

fn codecs_among(guids: &[GUID]) -> Vec<Codec> {
    Codec::ALL
        .into_iter()
        .filter(|&codec| guids.contains(&codec_guid(codec)))
        .collect()
}

const fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::CODEC_H264,
        Codec::Hevc => ffi::CODEC_HEVC,
        Codec::Av1 => ffi::CODEC_AV1,
    }
}

const fn profile_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::PROFILE_H264_HIGH,
        Codec::Hevc => ffi::PROFILE_HEVC_MAIN,
        Codec::Av1 => ffi::PROFILE_AV1_MAIN,
    }
}

fn configure(config: &mut ffi::Config, settings: Settings) {
    let matrix = u32::from(settings.matrix.code());
    let gop = gop_frames(settings.frame_rate);
    config.profile_guid = profile_guid(settings.codec);
    config.gop_length = gop;
    // No B-frames.
    config.frame_interval_p = 1;
    configure_rate_control(&mut config.rc, settings.bitrate_bps);
    match settings.codec {
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::H264 => configure_h264(unsafe { &mut config.codec.h264 }, gop, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Hevc => configure_hevc(unsafe { &mut config.codec.hevc }, gop, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Av1 => configure_av1(unsafe { &mut config.codec.av1 }, gop, matrix),
    }
}

/// Frames between keyframes, never fewer than one.
const fn gop_frames(frame_rate: u32) -> u32 {
    let frames = frame_rate / KEYFRAMES_PER_SECOND;
    if frames == 0 { 1 } else { frames }
}

/// Single-pass VBR with no lookahead or adaptive quantization: nothing runs on the shader cores.
fn configure_rate_control(rc: &mut ffi::RcParams, bitrate_bps: u32) {
    rc.rate_control_mode = ffi::RATE_CONTROL_VBR;
    rc.average_bit_rate = bitrate_bps;
    rc.max_bit_rate = bitrate_bps.saturating_mul(2);
    rc.vbv_buffer_size = rc.max_bit_rate;
    rc.flags = 0;
    rc.multi_pass = ffi::MULTI_PASS_DISABLED;
}

fn configure_h264(h264: &mut ffi::H264Config, gop: u32, matrix: u32) {
    h264.flags |= ffi::H264_REPEAT_SPS_PPS;
    h264.idr_period = gop;
    h264.chroma_format_idc = ffi::CHROMA_FORMAT_420;
    h264.vui.signal_colour(matrix);
}

/// Coding units held to 16×16–32×32 for speed.
fn configure_hevc(hevc: &mut ffi::HevcConfig, gop: u32, matrix: u32) {
    hevc.flags &= !(ffi::HEVC_CHROMA_FORMAT_MASK | ffi::HEVC_BIT_DEPTH_MASK);
    hevc.flags |=
        ffi::HEVC_REPEAT_SPS_PPS | (ffi::CHROMA_FORMAT_420 << ffi::HEVC_CHROMA_FORMAT_SHIFT);
    hevc.idr_period = gop;
    hevc.min_cu_size = ffi::HEVC_CU_SIZE_16;
    hevc.max_cu_size = ffi::HEVC_CU_SIZE_32;
    hevc.vui.signal_colour(matrix);
}

/// Partitions held to 16×16–32×32 for speed; plain OBUs without Annex B framing.
fn configure_av1(av1: &mut ffi::Av1Config, gop: u32, matrix: u32) {
    av1.flags &= !(ffi::AV1_ANNEX_B
        | ffi::AV1_TIMING_INFO
        | ffi::AV1_DECODER_MODEL_INFO
        | ffi::AV1_FRAME_ID_NUMBERS
        | ffi::AV1_DISABLE_SEQUENCE_HEADER
        | ffi::AV1_CHROMA_FORMAT_MASK
        | ffi::AV1_BIT_DEPTH_MASK);
    av1.flags |=
        ffi::AV1_REPEAT_SEQUENCE_HEADER | (ffi::CHROMA_FORMAT_420 << ffi::AV1_CHROMA_FORMAT_SHIFT);
    av1.idr_period = gop;
    av1.min_part_size = ffi::AV1_PART_SIZE_16;
    av1.max_part_size = ffi::AV1_PART_SIZE_32;
    av1.signal_colour(matrix);
}

/// `config` is pointed to, not copied; it must stay in place until the session is initialized.
fn initialize_params(settings: Settings, config: &mut ffi::Config) -> ffi::InitializeParams {
    let mut params = ffi::InitializeParams::zeroed();
    params.version = ffi::INITIALIZE_PARAMS_VER;
    params.encode_guid = codec_guid(settings.codec);
    params.preset_guid = ffi::PRESET_P4;
    params.encode_width = settings.width;
    params.encode_height = settings.height;
    params.dar_width = settings.width;
    params.dar_height = settings.height;
    params.frame_rate_num = settings.frame_rate;
    params.frame_rate_den = 1;
    params.enable_encode_async = 1;
    params.enable_ptd = 1;
    params.encode_config = &raw mut *config;
    params.max_encode_width = settings.width;
    params.max_encode_height = settings.height;
    params.tuning_info = ffi::TUNING_HIGH_QUALITY;
    params
}

fn create_input_texture(
    device: &ID3D11Device,
    settings: Settings,
) -> Result<ID3D11Texture2D, NvencError> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: settings.width,
        Height: settings.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0.cast_unsigned(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let texture_failed = |error| NvencError::Windows("create an encoder input texture", error);
    let mut texture = None;
    // SAFETY: a plain texture description without initial data.
    unsafe { device.CreateTexture2D(&raw const description, None, Some(&raw mut texture)) }
        .map_err(texture_failed)?;
    texture.ok_or_else(|| texture_failed(null_output()))
}

/// Twice the uncompressed picture, as NVIDIA's sample sizes it, plus the header NVENC writes first.
fn staging_bytes(settings: Settings) -> usize {
    let picture = settings.width as usize * settings.height as usize * 4;
    (2 * picture).max(STAGING_LAYOUT.head_bytes) + STAGING_LAYOUT.header_bytes
}

fn create_event() -> Result<Owned<HANDLE>, NvencError> {
    // SAFETY: an unnamed auto-reset event with default security.
    let event = unsafe { CreateEventW(None, false, false, None) }
        .map_err(|error| NvencError::Windows("create an encoder event", error))?;
    // SAFETY: the event was just created, and only this `Owned` closes it.
    Ok(unsafe { Owned::new(event) })
}

fn check(
    call: &'static str,
    status: Status,
    detail: impl FnOnce() -> String,
) -> Result<(), NvencError> {
    status.ok().map_err(|status| {
        if status == Status::INVALID_VERSION {
            return NvencError::VersionRejected;
        }
        NvencError::Call {
            call,
            status,
            detail: detail(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_come_twice_a_second_whatever_the_frame_rate() {
        assert_eq!(gop_frames(60), 30);
        assert_eq!(gop_frames(240), 120);
        assert_eq!(gop_frames(25), 12);
        assert_eq!(gop_frames(1), 1);
    }

    #[test]
    fn only_the_codecs_the_driver_lists_are_offered_in_the_usual_order() {
        assert_eq!(codecs_among(&[]), Vec::<Codec>::new());
        assert_eq!(
            codecs_among(&[ffi::CODEC_AV1, GUID::zeroed(), ffi::CODEC_H264]),
            vec![Codec::H264, Codec::Av1]
        );
        assert_eq!(codecs_among(&[ffi::CODEC_HEVC]), vec![Codec::Hevc]);
    }
}
