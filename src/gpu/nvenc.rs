//! Hardware encoding through NVENC (`nvEncodeAPI64.dll`): an asynchronous ring of slots fed
//! directly from video memory.

use crate::{
    config::Codec,
    gpu::{
        capture::{EncoderDevice, Format, Surface},
        cuda::{DeviceBuffer, DevicePointer},
        cuda_output::{CudaRing, StagingId, StagingLayout},
        device::{Locked, null_output},
        pacer::FrameIndex,
    },
    replay::{Matrix, Picture, Slot as StoreSlot},
};
use bytemuck::Zeroable;
use std::{collections::VecDeque, ffi::c_void, ptr};
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
    core::{GUID, Interface, Owned},
};

mod config;
#[cfg(test)]
#[expect(unsafe_code, reason = "calls Direct3D 12 and NVENC")]
mod d3d12_cost;
#[expect(unsafe_code, reason = "binds the NVENC API")]
mod ffi;
#[expect(unsafe_code, reason = "calls the NVENC function table")]
mod session;

use config::{codec_guid, configure, initialize_params};
pub(super) use ffi::Status;
use session::{Session, SessionDevice};

/// Two slots suffice while the GPU keeps up. The third absorbs a frame NVENC finishes late.
const SLOTS: usize = 3;
const ENCODE_TIMEOUT_MS: u32 = 2000;
/// The number of bitstream bytes read back with a frame's header, enough to recognise a keyframe.
const HEAD_BYTES: usize = 16 << 10;

/// The layout of a frame in a video-memory output buffer.
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
    #[error("move a frame into the replay buffer in GPU memory")]
    Ring(#[source] crate::replay::StoreError),
    #[error("the {0}-byte output buffer is larger than NVENC can register")]
    OutputTooLarge(usize),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    pub(super) codec: Codec,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) frame_rate: u32,
    pub(super) bitrate_bps: u32,
    pub(super) matrix: Matrix,
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

/// The output buffer each slot's frame is written to, indexed by slot.
enum Outputs {
    /// Buffers owned by the driver, which are locked to be read.
    Bitstreams(Vec<ffi::Bitstream>),
    /// Buffers in video memory, from which each frame is moved into the ring there. Only a CUDA
    /// session is given a ring.
    VideoMemory {
        ring: CudaRing,
        buffers: Vec<VideoOutput>,
    },
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
    outputs: Outputs,
    /// Declared last, so that everything registered with it is dropped first.
    session: Session,
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

/// This slot's frames are written to the slot's own entry in [`Outputs`].
struct Slot {
    texture: Option<ID3D11Texture2D>,
    registered: ffi::Registered,
    event: Owned<HANDLE>,
    in_flight: Option<Frame>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Poll,
    Block,
}

#[expect(unsafe_code, reason = "calls the NVENC session")]
impl Encoder {
    pub(super) fn open(
        device: &ID3D11Device,
        on: EncoderDevice,
        settings: Settings,
    ) -> Result<Self, NvencError> {
        let device_lock: ID3D11Multithread = device
            .cast()
            .map_err(|error| NvencError::Windows("lock the device", error))?;
        let bitstreams = || Outputs::Bitstreams(Vec::with_capacity(SLOTS));
        let (on, outputs) = match on {
            EncoderDevice::Direct3D => (SessionDevice::Direct3D11(device.clone()), bitstreams()),
            EncoderDevice::Cuda { hold, ring } => (
                SessionDevice::Cuda(hold),
                ring.map_or_else(bitstreams, |ring| Outputs::VideoMemory {
                    ring,
                    buffers: Vec::with_capacity(SLOTS),
                }),
            ),
            EncoderDevice::Direct3D9(device) => (SessionDevice::Direct3D9(device), bitstreams()),
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
            outputs,
            session: Session::open(on)?,
        };
        // From here on, `Drop` releases whatever has been created when a step fails.
        encoder.require_codec()?;
        encoder.require_size()?;
        encoder.initialize()?;
        for _ in 0..SLOTS {
            encoder.add_output()?;
            let slot = encoder.create_slot(device)?;
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
        let mut config = self.session.preset_config(self.settings.codec)?;
        configure(&mut config, self.settings);
        let mut params = initialize_params(self.settings);
        if matches!(self.outputs, Outputs::VideoMemory { .. }) {
            params.flags |= ffi::INITIALIZE_OUTPUT_IN_VIDMEM;
        }
        self.session.initialize(params, &mut config)
    }

    fn create_slot(&mut self, device: &ID3D11Device) -> Result<Slot, NvencError> {
        let texture = match self.session.device {
            SessionDevice::Direct3D11(_) => Some(create_input_texture(device, self.settings)?),
            _ => None,
        };
        let event = create_event()?;
        let mut slot = Slot {
            texture,
            registered: ptr::null_mut(),
            event,
            in_flight: None,
        };
        if let Err(error) = self.prepare_slot(&mut slot) {
            self.release_slot(&slot);
            return Err(error);
        }
        Ok(slot)
    }

    /// Creates the output for the next slot. `Drop` releases it if creating the slot itself
    /// fails.
    fn add_output(&mut self) -> Result<(), NvencError> {
        match &mut self.outputs {
            Outputs::Bitstreams(bitstreams) => bitstreams.push(self.session.create_bitstream()?),
            Outputs::VideoMemory { ring, buffers } => {
                let (staging, pointer) = ring
                    .allocate_staging(staging_bytes(self.settings))
                    .map_err(|error| NvencError::Cuda("allocate an output buffer", error))?;
                buffers.push(Self::register_output(
                    &self.session,
                    self.settings,
                    staging,
                    pointer,
                )?);
            }
        }
        Ok(())
    }

    fn prepare_slot(&self, slot: &mut Slot) -> Result<(), NvencError> {
        if let Some(texture) = &slot.texture {
            slot.registered = self.register_texture(texture)?;
        }
        self.register_event(&slot.event)
    }

    fn register_output(
        session: &Session,
        settings: Settings,
        staging: StagingId,
        pointer: DevicePointer,
    ) -> Result<VideoOutput, NvencError> {
        let bytes = staging_bytes(settings);
        let mut params = ffi::RegisterResource::zeroed();
        params.version = ffi::REGISTER_RESOURCE_VER;
        params.resource_type = ffi::INPUT_RESOURCE_TYPE_CUDA_DEVICE_POINTER;
        // A buffer of bytes is registered as one row of them, as NVIDIA's sample does.
        params.width = u32::try_from(bytes).map_err(|_| NvencError::OutputTooLarge(bytes))?;
        params.height = 1;
        params.pitch = params.width;
        params.resource_to_register = pointer.as_api_pointer();
        params.buffer_format = ffi::BUFFER_FORMAT_U8;
        params.buffer_usage = ffi::OUTPUT_BITSTREAM;
        // SAFETY: the ring keeps the buffer allocated until it is dropped, after `Drop`
        // unregisters it.
        let registered = unsafe { session.register_resource(&mut params) }?;
        // Mapped once, since one frame at a time writes the buffer.
        // SAFETY: registered just above and not mapped yet.
        match unsafe { session.map_input(registered) } {
            Ok(map) => Ok(VideoOutput {
                staging,
                registered,
                mapped: map.mapped_resource,
            }),
            Err(error) => {
                // SAFETY: registered above, not mapped, and never used again.
                unsafe { session.unregister_resource(registered) };
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
        format: Format,
    ) -> Result<ffi::Registered, NvencError> {
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_DIRECTX,
            surface.as_raw(),
            0,
            match format {
                Format::Bgra => ffi::BUFFER_FORMAT_ARGB,
                Format::Nv12 => ffi::BUFFER_FORMAT_NV12,
            },
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
        // SAFETY: the resource stays alive while registered: a slot's texture is owned by the
        // slot, a surface's handle is kept with its registration, and the capture outlives both.
        unsafe { self.session.register_resource(&mut params) }
    }

    fn register_event(&self, event: &Owned<HANDLE>) -> Result<(), NvencError> {
        // SAFETY: the event belongs to the slot, which unregisters it in `release_slot` before
        // the `Owned` closes it.
        unsafe { self.session.register_event(**event) }
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
                Surface::CudaBuffer { .. } | Surface::Direct3D9 { .. } => None,
            },
        }
    }

    pub(super) fn latest_surface(&self) -> Option<Surface> {
        match self.latest? {
            Input::Slot(slot) => self.slots[slot].texture.clone().map(Surface::Texture),
            Input::Surface(surface) => Some(self.registered_surface(surface).handle.clone()),
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
            Surface::Direct3D9 { surface, format } => {
                self.register_d3d9_surface(surface, *format)?
            }
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

    /// Returns `false` while NVENC is still reading the picture for the previous frame, because
    /// a texture is mapped for one frame at a time.
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
        // SAFETY: registered by this session and not mapped: `reserve`, `repeat` and
        // `release_surface` wait for the frame that last read it.
        let map = unsafe { self.session.map_input(registered) }?;
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
        picture.output_bitstream = match &self.outputs {
            Outputs::Bitstreams(bitstreams) => bitstreams[slot],
            Outputs::VideoMemory { buffers, .. } => buffers[slot].mapped,
        };
        picture.completion_event = self.slots[slot].event.0;
        picture.buffer_format = map.mapped_buffer_format;
        picture.picture_struct = ffi::PIC_STRUCT_FRAME;
        // SAFETY: the input was just mapped; the slot's output buffer is neither locked nor in
        // flight, since `reserve` collected its last frame; its event is registered.
        if let Err(error) = unsafe { self.session.encode_picture(&mut picture) } {
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

        // The device lock is released before `output` runs, because `output` takes the replay
        // buffer's lock.
        let result = match &mut self.outputs {
            Outputs::Bitstreams(bitstreams) => {
                let bitstream = bitstreams[slot];
                let _locked = Locked::new(&self.device_lock);
                let result = self.read_bitstream(bitstream);
                self.unmap(frame.mapped);
                result
            }
            Outputs::VideoMemory { ring, buffers } => {
                // Not under the device lock, because the move is a synchronous copy on the GPU
                // and the capture threads would wait for it.
                let result = ring
                    .take(buffers[slot].staging)
                    .map(|(slot, head)| (None, Payload::Stored { slot, head }))
                    .map_err(NvencError::Ring);
                let _locked = Locked::new(&self.device_lock);
                self.unmap(frame.mapped);
                result
            }
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
        // SAFETY: the slot's buffer, created by this session; its event has fired, so the
        // frame written into it is complete.
        let (picture_type, data) = unsafe { self.session.read_bitstream(bitstream) }?;
        Ok((Some(picture_type), Payload::Memory(data)))
    }

    /// `mapped` must be from a mapping made by this session that has not been unmapped, and the
    /// frame reading it must have completed or never have been queued.
    fn unmap(&self, mapped: ffi::Mapped) {
        // SAFETY: the callers ensure the preconditions above.
        unsafe { self.session.unmap_input(mapped) };
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
        let Some(slot) = self.slots.first() else {
            return;
        };
        // SAFETY: the slot's event is registered with this session and open.
        if unsafe { self.session.end_of_stream(*slot.event) }.is_ok() {
            // SAFETY: the slot's event is open.
            let _ = unsafe { WaitForSingleObject(*slot.event, ENCODE_TIMEOUT_MS) };
        }
    }

    /// Releases the outputs. Must be called after every slot's frame is released.
    fn release_outputs(&self) {
        match &self.outputs {
            Outputs::Bitstreams(bitstreams) => {
                for &bitstream in bitstreams {
                    // SAFETY: created by this session, unlocked, and no longer in flight.
                    unsafe { self.session.destroy_bitstream(bitstream) };
                }
            }
            Outputs::VideoMemory { buffers, .. } => {
                for output in buffers {
                    self.unmap(output.mapped);
                    // SAFETY: unmapped just above and no longer in flight; the ring frees it
                    // afterwards.
                    unsafe { self.session.unregister_resource(output.registered) };
                }
            }
        }
    }

    fn unregister_surfaces(&self) {
        for surface in self.surfaces.iter().flatten() {
            // SAFETY: registered by this session; every frame is collected or abandoned with the
            // session, so none is mapped; never used again.
            unsafe { self.session.unregister_resource(surface.registered) };
        }
    }

    fn release_slot(&self, slot: &Slot) {
        if let Some(frame) = &slot.in_flight {
            self.unmap(frame.mapped);
        }
        // SAFETY: the slot's event, still open. Unregistering one that was never registered is
        // reported, not fatal.
        unsafe { self.session.unregister_event(*slot.event) };
        if !slot.registered.is_null() {
            // SAFETY: the slot's texture, registered by this session and unmapped above.
            unsafe { self.session.unregister_resource(slot.registered) };
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
        self.release_outputs();
        self.unregister_surfaces();
        if self.stalled
            && let Outputs::VideoMemory { ring, .. } = &mut self.outputs
        {
            ring.leak_staging();
        }
        // Under the device lock, which `Drop` alone would not hold.
        self.session.destroy();
    }
}

/// The codecs this GPU's NVENC has an encoder for, in `Codec::ALL` order.
pub(super) fn supported_codecs(device: &ID3D11Device) -> Result<Vec<Codec>, NvencError> {
    let session = Session::open(SessionDevice::Direct3D11(device.clone()))?;
    Ok(codecs_among(&session.encode_guids()?))
}

fn codecs_among(guids: &[GUID]) -> Vec<Codec> {
    Codec::ALL
        .into_iter()
        .filter(|&codec| guids.contains(&codec_guid(codec)))
        .collect()
}

#[expect(unsafe_code, reason = "calls ID3D11Device::CreateTexture2D")]
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

#[expect(unsafe_code, reason = "calls CreateEventW")]
fn create_event() -> Result<Owned<HANDLE>, NvencError> {
    // SAFETY: an unnamed auto-reset event with default security.
    let event = unsafe { CreateEventW(None, false, false, None) }
        .map_err(|error| NvencError::Windows("create an encoder event", error))?;
    // SAFETY: the event was just created, and only this `Owned` closes it.
    Ok(unsafe { Owned::new(event) })
}

#[cfg(test)]
mod tests {
    use super::*;

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
