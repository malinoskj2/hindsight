//! Hardware encoding through NVIDIA's Video Codec SDK (`nvEncodeAPI64.dll`, installed with the
//! driver).
//!
//! The encoder runs asynchronously over a small ring of slots, each an input texture registered
//! with NVENC, an output buffer and a completion event. The capture thread fills a slot and
//! submits it, then picks up whichever earlier slots the GPU has finished; it only ever waits
//! when every slot is still in flight. Frames are encoded straight from video memory, and only
//! the compressed bitstream is copied to system memory.
//!
//! A capture that grabs into textures of its own can have them registered too
//! ([`Encoder::register_surface`]), so NVENC reads each picture where the capture left it
//! instead of from a copy in a slot's texture.
//!
//! The session runs on the shared Direct3D 11 device, or, for a capture that grabs into CUDA
//! memory, on that capture's CUDA context: then the slots have no textures of their own, and
//! every frame is read from one of the capture's surfaces. Such a session can also write each
//! frame's bitstream into a buffer of the application's in video memory ([`Output::VideoMemory`]),
//! from which it is moved into the replay buffer there without ever crossing to system memory.
//! Likewise, for NvFBC grabbing NV12, which Direct3D 9 holds only in plain surfaces that
//! Direct3D 11 cannot open, the session runs on NvFBC's Direct3D 9Ex device and reads the NV12
//! surfaces in place, signalling the BT.601 matrix NvFBC converted them with; NvFBC's ARGB
//! surfaces can be read the same way, on its device, when the capture asks for that.

use super::{
    capture::{EncoderDevice, Surface},
    cuda::DevicePointer,
    cuda_output::CudaRing,
};
use crate::{
    config::Codec,
    log,
    replay::{Matrix, Slot as StoreSlot},
};
use std::{collections::VecDeque, ffi::CStr, ptr};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HANDLE, HMODULE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::{
            Direct3D9::IDirect3DSurface9,
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
    core::{GUID, Interface, Owned, s, w},
};

mod ffi;
// A by-hand measurement of what each NVENC call costs on Direct3D 11 and Direct3D 12.
#[cfg(test)]
mod d3d12_cost;

// Re-exported because `NvencError::Call` carries it.
pub(super) use ffi::Status;
// Re-exported for the CUDA output ring, which reads the header the encoder writes.
use ffi::Zeroed;
pub(super) use ffi::{ENCODE_OUT_PARAMS_BYTES, ENCODE_OUT_PARAMS_SIZE_OFFSET};

/// Frames in flight at once. Two would do while the GPU keeps up; the third absorbs a frame
/// that NVENC finishes late because a game saturates the GPU.
const SLOTS: usize = 3;
/// How long to wait for a frame before deciding NVENC has stopped responding.
const ENCODE_TIMEOUT_MS: u32 = 2000;
/// Frames between keyframes, as in NVIDIA's Instant Replay at any frame rate. Saved clips start
/// within this many frames of the requested length.
const GOP_FRAMES: u32 = 30;

/// Why the encoder could not open, or stopped encoding.
#[derive(Debug, Error)]
pub(crate) enum NvencError {
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

/// What to encode.
#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    pub(super) codec: Codec,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) frame_rate: u32,
    pub(super) bitrate: u32,
    /// The matrix the input was converted to YUV with, or NVENC converts BGRA input with, which
    /// the bitstream signals.
    pub(super) matrix: Matrix,
}

/// Where the encoder puts the frames it finishes.
pub(super) enum Output {
    /// Each frame's bitstream is copied into system memory as it completes.
    Memory,
    /// Each frame's bitstream is written into video memory and moved into the ring there; only
    /// its first bytes come to system memory. For a CUDA session only.
    VideoMemory(CudaRing),
}

/// A finished frame.
pub(super) struct Encoded {
    pub(super) index: u64,
    pub(super) fresh: bool,
    /// The picture type NVENC reports, which it does only for a frame locked in system
    /// memory.
    picture_type: Option<u32>,
    pub(super) data: Payload,
}

/// A finished frame's bitstream, where the encoder put it.
pub(super) enum Payload {
    /// The whole bitstream.
    Memory(Box<[u8]>),
    /// The bitstream in the ring in video memory, with its first bytes.
    Stored { slot: StoreSlot, head: Box<[u8]> },
}

impl Encoded {
    /// Whether NVENC reports the frame as an instantaneous decoder refresh.
    pub(super) fn is_idr(&self) -> bool {
        self.picture_type == Some(ffi::PIC_TYPE_IDR)
    }

    /// The start of the bitstream, enough to tell a keyframe from its headers.
    pub(super) fn head(&self) -> &[u8] {
        match &self.data {
            Payload::Memory(data) => data,
            Payload::Stored { head, .. } => head,
        }
    }
}

/// The variable that has each finished frame's bitstream locked without waiting; see
/// [`lock_flags_from_environment`].
const LOCK_NO_WAIT_VARIABLE: &str = "HINDSIGHT_NVENC_LOCK_NOWAIT";

/// The flags each bitstream is locked with: `doNotWait` when `HINDSIGHT_NVENC_LOCK_NOWAIT=1`,
/// otherwise none, the default. A frame is only read once its completion event has fired, so
/// there is nothing left to wait for; measured on a static picture
/// (`d3d12_cost`), the lock and unlock cost about 30,000 cycles instead of 105,000, though the
/// unmap that follows rises by 15,000. Meant for measuring live before it becomes the default.
fn lock_flags_from_environment() -> u32 {
    let asked = std::env::var(LOCK_NO_WAIT_VARIABLE).is_ok_and(|value| value.trim() == "1");
    if asked {
        log::info(
            "NVENC locks each finished bitstream without waiting (HINDSIGHT_NVENC_LOCK_NOWAIT=1)",
        );
    }
    u32::from(asked)
}

/// A frame's output buffer in video memory, registered and mapped for the session.
struct VideoOutput {
    staging: DevicePointer,
    registered: ffi::Registered,
    mapped: ffi::Mapped,
}

/// An NVENC encode session and its ring of [`SLOTS`] slots.
pub(super) struct Encoder {
    api: Box<ffi::FunctionList>,
    session: ffi::Session,
    settings: Settings,
    slots: Vec<Slot>,
    /// The capture's own surfaces registered for NVENC to read in place, by the capture's
    /// index for each.
    surfaces: Vec<Option<RegisteredSurface>>,
    /// Slots submitted and not yet collected, oldest first.
    queue: VecDeque<usize>,
    /// The slot the next frame goes into.
    next: usize,
    /// The input of the newest submitted frame, whose texture holds the latest picture.
    latest: Option<Input>,
    /// The device's lock, held around calls that touch its resources: the capture frame pool
    /// works on the same device from its own threads.
    device_lock: ID3D11Multithread,
    /// NVENC stopped answering; teardown does not wait for it again.
    stalled: bool,
    /// The flags each finished frame's bitstream is locked with.
    lock_flags: u32,
    /// The ring in video memory the frames go to, when they do; dropped after the slots have
    /// let go of its staging buffers.
    ring: Option<CudaRing>,
    /// Declared last, so it is freed after everything else the encoder holds.
    library: Owned<HMODULE>,
}

/// A submitted frame, which keeps its input mapped until it is collected.
struct Frame {
    index: u64,
    fresh: bool,
    input: Input,
    mapped: ffi::Mapped,
}

/// Where a frame's picture is read from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    /// The texture of the slot with this index.
    Slot(usize),
    /// The capture's surface with this index.
    Surface(usize),
}

/// A surface of the capture's, registered with NVENC for the whole session.
struct RegisteredSurface {
    handle: Surface,
    registered: ffi::Registered,
}

/// One input texture and what NVENC needs to encode it. The encoder releases the NVENC parts in
/// [`Encoder::release_slot`]; the texture and the event are released when the slot is dropped.
struct Slot {
    /// The texture a frame is drawn into; none on a CUDA session, which reads the capture's
    /// surfaces only.
    texture: Option<ID3D11Texture2D>,
    registered: ffi::Registered,
    /// The output buffer NVENC owns, in system memory; null when the frames go to video
    /// memory instead.
    bitstream: ffi::Bitstream,
    /// The output buffer in video memory, when the frames go there.
    output: Option<VideoOutput>,
    event: Owned<HANDLE>,
    in_flight: Option<Frame>,
}

/// Whether collecting the oldest frame in flight may wait for NVENC to finish it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// Report at once that the frame is not ready.
    Poll,
    /// Wait up to [`ENCODE_TIMEOUT_MS`], then consider NVENC stalled.
    Block,
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
    /// Opens an encode session on `on`, the device the capture's surfaces are reachable from,
    /// and prepares its slots. `device` is the shared Direct3D 11 device, whose lock is held
    /// around the session's calls whichever device the session runs on.
    pub(super) fn open(
        device: &ID3D11Device,
        on: &EncoderDevice,
        settings: Settings,
        output: Output,
    ) -> Result<Self, NvencError> {
        let device_lock: ID3D11Multithread = device
            .cast()
            .map_err(|error| NvencError::Windows("lock the device", error))?;
        let library = load_library()?;
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
            api: Box::new(ffi::FunctionList::zeroed()),
            session: ptr::null_mut(),
            settings,
            slots: Vec::with_capacity(SLOTS),
            surfaces: Vec::new(),
            queue: VecDeque::with_capacity(SLOTS),
            next: 0,
            latest: None,
            device_lock,
            stalled: false,
            lock_flags: lock_flags_from_environment(),
            ring,
            library,
        };
        // From here on, `Drop` releases whatever has been created when a step fails.
        encoder.load_api()?;
        encoder.open_session(device, on)?;
        encoder.require_codec()?;
        encoder.require_size()?;
        encoder.initialize()?;
        for _ in 0..SLOTS {
            let slot = encoder.create_slot(device, on)?;
            encoder.slots.push(slot);
        }
        Ok(encoder)
    }

    /// The settings the encoder was opened with.
    pub(super) const fn settings(&self) -> Settings {
        self.settings
    }

    /// Fills the function list from the library, once the driver has shown that it supports the
    /// API version these structures follow.
    fn load_api(&mut self) -> Result<(), NvencError> {
        let (max_version, create_instance) = resolve_exports(*self.library)?;
        self.require_driver_version(max_version)?;
        self.api.version = ffi::FUNCTION_LIST_VER;
        // SAFETY: the list is a properly versioned, writable `NV_ENCODE_API_FUNCTION_LIST`.
        let status = unsafe { create_instance(&raw mut *self.api) };
        self.check("NvEncodeAPICreateInstance", status)
    }

    /// Fails when the driver's newest NVENC API is older than the one this encoder speaks.
    fn require_driver_version(
        &self,
        max_version: ffi::GetMaxSupportedVersion,
    ) -> Result<(), NvencError> {
        let mut version = 0;
        // SAFETY: writes one integer.
        self.check("NvEncodeAPIGetMaxSupportedVersion", unsafe {
            max_version(&raw mut version)
        })?;
        if version < ffi::REQUIRED_MAX_VERSION {
            let (major, minor) = ffi::split_max_version(version);
            return Err(NvencError::DriverTooOld(major, minor));
        }
        Ok(())
    }

    /// Opens the encode session on `on`, destroying whatever half-open session a failed
    /// attempt leaves behind.
    fn open_session(
        &mut self,
        device: &ID3D11Device,
        on: &EncoderDevice,
    ) -> Result<(), NvencError> {
        let mut params = ffi::OpenEncodeSessionExParams::zeroed();
        params.version = ffi::OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        (params.device_type, params.device) = match on {
            EncoderDevice::Direct3D => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
            EncoderDevice::Cuda { context, .. } => (ffi::DEVICE_TYPE_CUDA, context.as_ptr()),
            EncoderDevice::Direct3D9(device) => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
        };
        params.api_version = ffi::API_VERSION;
        let open = entry_point(self.api.open_encode_session_ex, "nvEncOpenEncodeSessionEx")?;
        // SAFETY: the device or context outlives the session (the pipeline drops the encoder
        // before the device and the capture), and the session handle is written on success
        // only.
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

    /// Fails when this GPU's NVENC has no encoder for the codec.
    fn require_codec(&self) -> Result<(), NvencError> {
        let count_guids = entry_point(self.api.get_encode_guid_count, "nvEncGetEncodeGUIDCount")?;
        let list_guids = entry_point(self.api.get_encode_guids, "nvEncGetEncodeGUIDs")?;
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

    /// The value the codec's encoder reports for `capability`, which must be a non-negative
    /// limit.
    fn capability(&self, capability: u32) -> Result<u32, NvencError> {
        let mut query = ffi::CapsParam::zeroed();
        query.version = ffi::CAPS_PARAM_VER;
        query.caps_to_query = capability;
        let get_caps = entry_point(self.api.get_encode_caps, "nvEncGetEncodeCaps")?;
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
        u32::try_from(value).map_err(|_| NvencError::BadCapability { capability, value })
    }

    /// Initializes the session with the preset's configuration, adapted by [`configure`].
    fn initialize(&self) -> Result<(), NvencError> {
        let mut config = self.preset_config()?;
        configure(&mut config, self.settings);
        let mut params = initialize_params(self.settings, &mut config);
        if self.ring.is_some() {
            params.flags |= ffi::INITIALIZE_OUTPUT_IN_VIDMEM;
        }
        let initialize = entry_point(self.api.initialize_encoder, "nvEncInitializeEncoder")?;
        // SAFETY: `params` and the configuration it points to live across the call.
        self.check("nvEncInitializeEncoder", unsafe {
            initialize(self.session, &raw mut params)
        })
    }

    /// The configuration the P4 preset, tuned for high quality, gives the codec.
    fn preset_config(&self) -> Result<ffi::Config, NvencError> {
        let mut preset = ffi::PresetConfig::zeroed();
        preset.version = ffi::PRESET_CONFIG_VER;
        preset.config.version = ffi::CONFIG_VER;
        let get_preset = entry_point(
            self.api.get_encode_preset_config_ex,
            "nvEncGetEncodePresetConfigEx",
        )?;
        // SAFETY: the preset structure is versioned and writable.
        self.check("nvEncGetEncodePresetConfigEx", unsafe {
            get_preset(
                self.session,
                codec_guid(self.settings.codec),
                ffi::PRESET_P4,
                ffi::TUNING_HIGH_QUALITY,
                &raw mut preset,
            )
        })?;
        Ok(preset.config)
    }

    /// Creates one slot, with an input texture when the session is on the Direct3D device,
    /// releasing what it registered with NVENC if a step fails.
    fn create_slot(
        &mut self,
        device: &ID3D11Device,
        on: &EncoderDevice,
    ) -> Result<Slot, NvencError> {
        let texture = match on {
            EncoderDevice::Direct3D => Some(create_input_texture(device, self.settings)?),
            EncoderDevice::Cuda { .. } | EncoderDevice::Direct3D9(_) => None,
        };
        let staging = match self.ring.as_mut() {
            Some(ring) => Some(
                ring.allocate_staging(staging_bytes(self.settings))
                    .map_err(|error| NvencError::Cuda("allocate an output buffer", error))?,
            ),
            None => None,
        };
        let event = create_event()?;
        let mut slot = Slot {
            texture,
            registered: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            output: None,
            event,
            in_flight: None,
        };
        if let Err(error) = self.prepare_slot(&mut slot, staging) {
            self.release_slot(&slot);
            return Err(error);
        }
        Ok(slot)
    }

    /// Registers the slot's texture, output buffer (the one NVENC makes in system memory, or
    /// the `staging` buffer in video memory) and event with the session, storing each handle
    /// in the slot as soon as it exists so that [`release_slot`](Self::release_slot) can undo
    /// a partial setup.
    fn prepare_slot(
        &self,
        slot: &mut Slot,
        staging: Option<DevicePointer>,
    ) -> Result<(), NvencError> {
        if let Some(texture) = &slot.texture {
            slot.registered = self.register_texture(texture)?;
        }
        match staging {
            Some(staging) => slot.output = Some(self.register_output(staging)?),
            None => slot.bitstream = self.create_bitstream()?,
        }
        self.register_event(&slot.event)
    }

    /// Registers the buffer at `staging` in the session's context as one NVENC writes a
    /// frame's bitstream to, and maps it for the session.
    fn register_output(&self, staging: DevicePointer) -> Result<VideoOutput, NvencError> {
        let bytes = staging_bytes(self.settings);
        let mut params = ffi::RegisterResource::zeroed();
        params.version = ffi::REGISTER_RESOURCE_VER;
        params.resource_type = ffi::INPUT_RESOURCE_TYPE_CUDA_DEVICE_POINTER;
        // A buffer of bytes is registered as one row of them, as NVIDIA's sample does.
        params.width = u32::try_from(bytes).expect("an output buffer fits 32 bits");
        params.height = 1;
        params.pitch = params.width;
        // A device address is not a pointer into this process; the API merely types it as
        // one.
        params.resource_to_register = ptr::without_provenance_mut(
            usize::try_from(staging).expect("a device address fits a 64-bit pointer"),
        );
        params.buffer_format = ffi::BUFFER_FORMAT_U8;
        params.buffer_usage = ffi::OUTPUT_BITSTREAM;
        let register = entry_point(self.api.register_resource, "nvEncRegisterResource")?;
        // SAFETY: the buffer stays allocated, and registered, as long as the slot that holds
        // it.
        self.check("nvEncRegisterResource", unsafe {
            register(self.session, &raw mut params)
        })?;
        let registered = params.registered_resource;
        // Mapped once for the session: the same buffer is written by one frame at a time, the
        // one in flight in its slot.
        match self.map_input(registered) {
            Ok(map) => Ok(VideoOutput {
                staging,
                registered,
                mapped: map.mapped_resource,
            }),
            Err(error) => {
                if let Some(unregister) = self.api.unregister_resource {
                    // SAFETY: the resource is registered and not mapped.
                    let _ = unsafe { unregister(self.session, registered) };
                }
                Err(error)
            }
        }
    }

    /// Registers `texture`, on the session's Direct3D device, as an input NVENC can map.
    fn register_texture(&self, texture: &ID3D11Texture2D) -> Result<ffi::Registered, NvencError> {
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_DIRECTX,
            texture.as_raw(),
            0,
            ffi::BUFFER_FORMAT_ARGB,
        )
    }

    /// Registers `surface`, NV12 if `nv12` and 32-bit BGRA otherwise, on the session's
    /// Direct3D 9Ex device, as an input NVENC can map.
    fn register_d3d9_surface(
        &self,
        surface: &IDirect3DSurface9,
        nv12: bool,
    ) -> Result<ffi::Registered, NvencError> {
        let format = if nv12 {
            ffi::BUFFER_FORMAT_NV12
        } else {
            ffi::BUFFER_FORMAT_ARGB
        };
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_DIRECTX,
            surface.as_raw(),
            0,
            format,
        )
    }

    /// Registers the CUDA buffer at `pointer`, in the session's context, with rows `pitch`
    /// bytes apart, as an input NVENC can map.
    fn register_cuda_buffer(
        &self,
        pointer: u64,
        pitch: u32,
    ) -> Result<ffi::Registered, NvencError> {
        // A device address is not a pointer into this process; the API merely types it as
        // one.
        let resource = ptr::without_provenance_mut(
            usize::try_from(pointer).expect("a device address fits a 64-bit pointer"),
        );
        self.register_resource(
            ffi::INPUT_RESOURCE_TYPE_CUDA_DEVICE_POINTER,
            resource,
            pitch,
            ffi::BUFFER_FORMAT_ARGB,
        )
    }

    /// Registers `resource` of `resource_type`, holding pictures of the encoded size in
    /// `buffer_format` with rows `pitch` bytes apart where the type needs telling, as an input
    /// NVENC can map.
    fn register_resource(
        &self,
        resource_type: u32,
        resource: *mut std::ffi::c_void,
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
        let register = entry_point(self.api.register_resource, "nvEncRegisterResource")?;
        // SAFETY: the resource stays alive, and registered, as long as the slot or surface
        // that holds it.
        self.check("nvEncRegisterResource", unsafe {
            register(self.session, &raw mut params)
        })?;
        Ok(params.registered_resource)
    }

    /// Creates an output buffer for one frame's bitstream.
    fn create_bitstream(&self) -> Result<ffi::Bitstream, NvencError> {
        let mut buffer = ffi::CreateBitstreamBuffer::zeroed();
        buffer.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
        let create = entry_point(
            self.api.create_bitstream_buffer,
            "nvEncCreateBitstreamBuffer",
        )?;
        // SAFETY: a versioned structure for an initialized session.
        self.check("nvEncCreateBitstreamBuffer", unsafe {
            create(self.session, &raw mut buffer)
        })?;
        Ok(buffer.bitstream_buffer)
    }

    /// Registers `event` to be signalled when a frame encoded with it completes.
    fn register_event(&self, event: &Owned<HANDLE>) -> Result<(), NvencError> {
        let mut params = ffi::EventParams::zeroed();
        params.version = ffi::EVENT_PARAMS_VER;
        params.completion_event = event.0;
        let register_event = entry_point(self.api.register_async_event, "nvEncRegisterAsyncEvent")?;
        // SAFETY: the event stays open until it is unregistered with the slot.
        self.check("nvEncRegisterAsyncEvent", unsafe {
            register_event(self.session, &raw mut params)
        })
    }

    /// Frees the slot the next frame goes into: while its own frame, or a repeat read from its
    /// texture, is still in flight, waits for the oldest frame and hands its packet to `output`.
    pub(super) fn reserve(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.slots[self.next].in_flight.is_some() || self.reads(Input::Slot(self.next)) {
            self.collect_one(Wait::Block, output)?;
        }
        Ok(())
    }

    /// The texture the next frame must be drawn into, once [`reserve`](Self::reserve) has freed
    /// its slot; none on a CUDA or Direct3D 9 session, which only reads the capture's surfaces.
    pub(super) fn input(&self) -> Option<&ID3D11Texture2D> {
        self.slots[self.next].texture.as_ref()
    }

    /// Whether a picture has been submitted, which a tick without a new one repeats.
    pub(super) const fn has_latest(&self) -> bool {
        self.latest.is_some()
    }

    /// The texture holding the newest submitted picture, to copy when it cannot be mapped
    /// again; none when the newest picture is in CUDA memory or on a Direct3D 9 device.
    pub(super) fn latest_texture(&self) -> Option<&ID3D11Texture2D> {
        match self.latest? {
            Input::Slot(slot) => self.slots[slot].texture.as_ref(),
            Input::Surface(surface) => match &self.registered_surface(surface).handle {
                Surface::Texture(texture) => Some(texture),
                Surface::CudaBuffer { .. } | Surface::Direct3D9 { .. } => None,
            },
        }
    }

    /// The capture's surface `surface`, which must have been registered.
    fn registered_surface(&self, surface: usize) -> &RegisteredSurface {
        self.surfaces[surface]
            .as_ref()
            .expect("only registered surfaces are submitted")
    }

    /// Whether the capture's surface `surface` is registered.
    pub(super) fn has_surface(&self, surface: usize) -> bool {
        self.surfaces.get(surface).is_some_and(Option::is_some)
    }

    /// Registers `handle`, the capture's surface `surface` holding pictures of the encoded
    /// size, so that [`submit_surface`](Self::submit_surface) can encode it where it is. A
    /// surface on the wrong device for the session is refused by NVENC.
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
            Surface::CudaBuffer { pointer, pitch } => {
                self.register_cuda_buffer(*pointer, *pitch)?
            }
            Surface::Direct3D9 { surface, nv12 } => self.register_d3d9_surface(surface, *nv12)?,
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

    /// Whether the encoder still needs the capture's surface `surface`: a frame in flight is
    /// read from it, or it holds the latest picture, which a repeat reads again.
    pub(super) fn holds_surface(&self, surface: usize) -> bool {
        self.latest == Some(Input::Surface(surface)) || self.reads(Input::Surface(surface))
    }

    /// Waits until NVENC has finished reading the capture's surface `surface`, handing every
    /// frame collected meanwhile to `output`, so that the capture may draw into it again.
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

    /// Whether a frame in flight is read from `input`.
    fn reads(&self, input: Input) -> bool {
        self.queue.iter().any(|&slot| {
            self.slots[slot]
                .in_flight
                .as_ref()
                .is_some_and(|frame| frame.input == input)
        })
    }

    /// Encodes the texture [`input`](Self::input) returned as frame `index`.
    pub(super) fn submit(&mut self, index: u64, fresh: bool) -> Result<(), NvencError> {
        self.submit_input(index, fresh, Input::Slot(self.next))
    }

    /// Encodes the new picture in the capture's surface `surface`, registered with
    /// [`register_surface`](Self::register_surface), as frame `index`.
    pub(super) fn submit_surface(&mut self, index: u64, surface: usize) -> Result<(), NvencError> {
        self.submit_input(index, true, Input::Surface(surface))
    }

    /// Encodes the newest submitted picture again as frame `index`, reading it where it is, and
    /// reports whether it could: not while NVENC is still reading it for the frame before, since
    /// a texture is mapped for one frame at a time. The caller then copies the picture into
    /// [`input`](Self::input) and submits that instead.
    pub(super) fn repeat(&mut self, index: u64) -> Result<bool, NvencError> {
        let Some(latest) = self.latest else {
            return Ok(false);
        };
        if self.reads(latest) {
            return Ok(false);
        }
        self.submit_input(index, false, latest)?;
        Ok(true)
    }

    /// Encodes the newest submitted picture again as frame `index`, reading it where it is,
    /// after waiting for NVENC to finish reading it for the frame before, handing every frame
    /// collected meanwhile to `output`. For a picture that cannot be copied instead, since it
    /// is in CUDA memory or on a Direct3D 9 device; the wait is at most one frame's encode.
    pub(super) fn repeat_once_free(
        &mut self,
        index: u64,
        output: &mut impl FnMut(Encoded),
    ) -> Result<(), NvencError> {
        let latest = self
            .latest
            .expect("a repeat is asked for only once a picture has been submitted");
        while self.reads(latest) {
            self.collect_one(Wait::Block, output)?;
        }
        self.submit_input(index, false, latest)
    }

    /// Encodes `input` as frame `index` into the next slot's output buffer.
    fn submit_input(&mut self, index: u64, fresh: bool, input: Input) -> Result<(), NvencError> {
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
            fresh,
            input,
            mapped: map.mapped_resource,
        });
        self.queue.push_back(slot);
        self.latest = Some(input);
        self.next = (slot + 1) % SLOTS;
        Ok(())
    }

    /// Maps the `registered` texture so NVENC can read it.
    fn map_input(&self, registered: ffi::Registered) -> Result<ffi::MapInputResource, NvencError> {
        let mut map = ffi::MapInputResource::zeroed();
        map.version = ffi::MAP_INPUT_RESOURCE_VER;
        map.registered_resource = registered;
        let map_input = entry_point(self.api.map_input_resource, "nvEncMapInputResource")?;
        // SAFETY: the resource is registered and not currently mapped: a texture is mapped
        // only while the one frame read from it is in flight (`reserve` and `repeat` check
        // that first), and the capture does not draw into a surface again before
        // `release_surface` or `holds_surface` says the encoder is done with it.
        self.check("nvEncMapInputResource", unsafe {
            map_input(self.session, &raw mut map)
        })?;
        Ok(map)
    }

    /// Queues the input `map` holds for encoding into `slot` as frame `index`, unmapping it
    /// again when NVENC refuses it or the driver lacks the call.
    fn encode_picture(
        &self,
        slot: usize,
        index: u64,
        map: &ffi::MapInputResource,
    ) -> Result<(), NvencError> {
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.input_width = self.settings.width;
        picture.input_height = self.settings.height;
        picture.input_time_stamp = index;
        picture.input_buffer = map.mapped_resource;
        picture.output_bitstream = match &self.slots[slot].output {
            Some(output) => output.mapped,
            None => self.slots[slot].bitstream,
        };
        picture.completion_event = self.slots[slot].event.0;
        picture.buffer_format = map.mapped_buffer_format;
        picture.picture_struct = ffi::PIC_STRUCT_FRAME;
        let queued =
            entry_point(self.api.encode_picture, "nvEncEncodePicture").and_then(|encode| {
                // SAFETY: the input is mapped and the output buffer is not locked or in flight.
                let status = unsafe { encode(self.session, &raw mut picture) };
                self.check("nvEncEncodePicture", status)
            });
        if let Err(error) = queued {
            self.unmap(map.mapped_resource);
            return Err(error);
        }
        Ok(())
    }

    /// Hands every frame NVENC has finished, oldest first, to `output` without waiting.
    pub(super) fn collect(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(Wait::Poll, output)? {}
        Ok(())
    }

    /// Waits for every frame in flight and hands them to `output`.
    pub(super) fn finish(&mut self, output: &mut impl FnMut(Encoded)) -> Result<(), NvencError> {
        while self.collect_one(Wait::Block, output)? {}
        Ok(())
    }

    /// Collects the oldest frame in flight; reports whether there was one ready.
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
                Some(output) => take_from_video_memory(self.ring.as_mut(), output.staging),
                None => self.read_bitstream(slot),
            };
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

    /// Waits as long as `wait` allows for the frame in `slot` to complete; reports whether it
    /// has, and marks the encoder stalled when a blocking wait runs out.
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

    /// Copies the completed frame out of the output buffer of `slot`, with the picture type
    /// NVENC chose for it.
    fn read_bitstream(&self, slot: usize) -> Result<(Option<u32>, Payload), NvencError> {
        let mut lock = ffi::LockBitstream::zeroed();
        lock.version = ffi::LOCK_BITSTREAM_VER;
        lock.output_bitstream = self.slots[slot].bitstream;
        lock.flags = self.lock_flags;
        let lock_bitstream = entry_point(self.api.lock_bitstream, "nvEncLockBitstream")?;
        let unlock_bitstream = entry_point(self.api.unlock_bitstream, "nvEncUnlockBitstream")?;
        // SAFETY: the buffer's frame has completed; it stays locked only while it is copied.
        self.check("nvEncLockBitstream", unsafe {
            lock_bitstream(self.session, &raw mut lock)
        })?;
        let data: Box<[u8]> = if lock.bitstream_buffer_ptr.is_null() {
            // A frame without output; a slice may not be made from a null pointer.
            Box::default()
        } else {
            // SAFETY: while locked, the driver guarantees `bitstream_size_in_bytes` readable
            // bytes at `bitstream_buffer_ptr`, which is not null.
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
            unlock_bitstream(self.session, self.slots[slot].bitstream)
        })?;
        Ok((Some(lock.picture_type), Payload::Memory(data)))
    }

    /// Undoes a successful [`map_input`](Self::map_input).
    fn unmap(&self, mapped: ffi::Mapped) {
        if let Some(unmap) = self.api.unmap_input_resource {
            // SAFETY: `mapped` came from a successful map that has not been undone.
            let _ = unsafe { unmap(self.session, mapped) };
        }
    }

    /// Turns the `status` of `call` into an error that carries the session's description of it.
    fn check(&self, call: &'static str, status: Status) -> Result<(), NvencError> {
        if status == Status::SUCCESS {
            return Ok(());
        }
        Err(call_error(call, status, self.last_error_detail()))
    }

    /// The session's description of its last error as a parenthesized suffix, or nothing when
    /// there is no session or no description.
    fn last_error_detail(&self) -> String {
        self.api
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
            .unwrap_or_default()
    }

    /// Gives each frame still in flight up to [`ENCODE_TIMEOUT_MS`] to complete, so the driver
    /// is done with its buffers before they are released.
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
            let _ = unsafe { WaitForSingleObject(*slot.event, ENCODE_TIMEOUT_MS) };
        }
    }

    /// Unregisters the capture's surfaces, none of which may still be mapped.
    fn unregister_surfaces(&self) {
        let Some(unregister) = self.api.unregister_resource else {
            return;
        };
        for surface in self.surfaces.iter().flatten() {
            // SAFETY: the resource is registered and unmapped.
            let _ = unsafe { unregister(self.session, surface.registered) };
        }
    }

    /// Undoes [`prepare_slot`](Self::prepare_slot) in reverse, after unmapping the input of a
    /// frame still in flight; each step is skipped when its part was never created.
    fn release_slot(&self, slot: &Slot) {
        if let Some(frame) = &slot.in_flight {
            self.unmap(frame.mapped);
        }
        if let Some(unregister_event) = self.api.unregister_async_event {
            let mut params = ffi::EventParams::zeroed();
            params.version = ffi::EVENT_PARAMS_VER;
            params.completion_event = slot.event.0;
            // SAFETY: unregistering an event that is not registered is reported, not fatal.
            let _ = unsafe { unregister_event(self.session, &raw mut params) };
        }
        if !slot.bitstream.is_null()
            && let Some(destroy) = self.api.destroy_bitstream_buffer
        {
            // SAFETY: the buffer is unlocked and no longer in flight.
            let _ = unsafe { destroy(self.session, slot.bitstream) };
        }
        if let Some(output) = &slot.output {
            self.unmap(output.mapped);
            if let Some(unregister) = self.api.unregister_resource {
                // SAFETY: the buffer is unmapped and no longer in flight; the ring frees it
                // afterwards.
                let _ = unsafe { unregister(self.session, output.registered) };
            }
        }
        if !slot.registered.is_null()
            && let Some(unregister) = self.api.unregister_resource
        {
            // SAFETY: the resource is unmapped.
            let _ = unsafe { unregister(self.session, slot.registered) };
        }
    }
}

impl Drop for Encoder {
    // After this body, the fields release the slots' textures and events, and the library last.
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        // Frames still in flight are abandoned; their buffers are released below. A stalled
        // encoder is not waited for again.
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
        // Every frame in flight was unmapped with its slot.
        self.unregister_surfaces();
        if let Some(destroy) = self.api.destroy_encoder {
            // SAFETY: every buffer and registration of the session is gone.
            let _ = unsafe { destroy(self.session) };
        }
    }
}

/// Moves the completed frame in the output buffer at `staging` into `ring`, the one in video
/// memory that the encoder has whenever a slot has such a buffer. NVENC reports no picture type
/// this way; the keyframes are told from the bitstream's first bytes instead.
fn take_from_video_memory(
    ring: Option<&mut CudaRing>,
    staging: DevicePointer,
) -> Result<(Option<u32>, Payload), NvencError> {
    let ring = ring.expect("a slot has an output buffer in video memory only with a ring");
    let (slot, head) = ring.take(staging).map_err(NvencError::Ring)?;
    Ok((None, Payload::Stored { slot, head }))
}

/// Loads the NVENC library the driver installs.
fn load_library() -> Result<Owned<HMODULE>, NvencError> {
    // The driver installs the library in System32; never look anywhere a planted copy could be.
    // SAFETY: loads a system library by name.
    let library =
        unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|_| NvencError::Library)?;
    // SAFETY: the module was just loaded, and only this `Owned` frees it.
    Ok(unsafe { Owned::new(library) })
}

/// The library's two exports: the version query and the function list's constructor.
fn resolve_exports(
    library: HMODULE,
) -> Result<(ffi::GetMaxSupportedVersion, ffi::CreateInstance), NvencError> {
    // SAFETY: looks a symbol up in the loaded NVENC library.
    let max_version = unsafe { GetProcAddress(library, s!("NvEncodeAPIGetMaxSupportedVersion")) }
        .ok_or(NvencError::Library)?;
    // SAFETY: looks a symbol up in the loaded NVENC library.
    let create_instance = unsafe { GetProcAddress(library, s!("NvEncodeAPICreateInstance")) }
        .ok_or(NvencError::Library)?;
    // SAFETY: nvEncodeAPI.h declares this export as `NVENCSTATUS NVENCAPI (uint32_t*)`, which
    // is `ffi::GetMaxSupportedVersion`.
    let max_version = unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::GetMaxSupportedVersion>(
            max_version,
        )
    };
    // SAFETY: nvEncodeAPI.h declares this export as
    // `NVENCSTATUS NVENCAPI (NV_ENCODE_API_FUNCTION_LIST*)`, which is `ffi::CreateInstance`.
    let create_instance = unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::CreateInstance>(
            create_instance,
        )
    };
    Ok((max_version, create_instance))
}

/// The function list entry for `call`, which a driver may leave empty.
fn entry_point<T>(entry: Option<T>, call: &'static str) -> Result<T, NvencError> {
    entry.ok_or(NvencError::MissingEntryPoint(call))
}

/// The GUID NVENC names the encoder for `codec` by.
const fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::CODEC_H264,
        Codec::Hevc => ffi::CODEC_HEVC,
        Codec::Av1 => ffi::CODEC_AV1,
    }
}

/// The profile `codec` is encoded in: High for H.264, Main for HEVC and AV1.
const fn profile_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => ffi::PROFILE_H264_HIGH,
        Codec::Hevc => ffi::PROFILE_HEVC_MAIN,
        Codec::Av1 => ffi::PROFILE_AV1_MAIN,
    }
}

/// Adapts the preset's configuration the way NVIDIA's Instant Replay sets up its encoder: the
/// codec's [profile](profile_guid), a keyframe every 30 frames, no B-frames, and the rate
/// control of [`configure_rate_control`]. Each codec's own settings make the picture 8-bit
/// 4:2:0 with BT.709 primaries and transfer and the input's matrix signalled, and repeat the
/// sequence header on every keyframe so any of them can start a clip.
fn configure(config: &mut ffi::Config, settings: Settings) {
    let matrix = u32::from(settings.matrix.code());
    config.profile_guid = profile_guid(settings.codec);
    config.gop_length = GOP_FRAMES;
    // Every frame after a keyframe is a P-frame: no B-frames.
    config.frame_interval_p = 1;
    configure_rate_control(&mut config.rc, settings.bitrate);
    match settings.codec {
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::H264 => configure_h264(unsafe { &mut config.codec.h264 }, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Hevc => configure_hevc(unsafe { &mut config.codec.hevc }, matrix),
        // SAFETY: the preset filled the union member for the configured codec.
        Codec::Av1 => configure_av1(unsafe { &mut config.codec.av1 }, matrix),
    }
}

/// Sets single-pass VBR averaging `bitrate` and peaking at twice that, with a buffer of the
/// same size, and no rate-control extras (lookahead, adaptive quantization): frames leave the
/// encoder in order, one per input, and nothing runs on the shader cores.
fn configure_rate_control(rc: &mut ffi::RcParams, bitrate: u32) {
    rc.rate_control_mode = ffi::RATE_CONTROL_VBR;
    rc.average_bit_rate = bitrate;
    rc.max_bit_rate = bitrate.saturating_mul(2);
    rc.vbv_buffer_size = rc.max_bit_rate;
    rc.flags = 0;
    rc.multi_pass = ffi::MULTI_PASS_DISABLED;
}

/// Makes H.264 4:2:0 with limited range, BT.709 primaries and transfer and `matrix` signalled,
/// an IDR on every keyframe and the parameter sets repeated before each.
fn configure_h264(h264: &mut ffi::H264Config, matrix: u32) {
    h264.flags |= ffi::H264_REPEAT_SPS_PPS;
    h264.idr_period = GOP_FRAMES;
    h264.chroma_format_idc = ffi::CHROMA_FORMAT_420;
    h264.vui.signal_colour(matrix);
}

/// Makes HEVC 8-bit 4:2:0 with limited range, BT.709 primaries and transfer and `matrix`
/// signalled, an IDR on every keyframe and the parameter sets repeated before each, held to
/// 16×16 to 32×32 coding units, which keeps the encoder fast.
fn configure_hevc(hevc: &mut ffi::HevcConfig, matrix: u32) {
    hevc.flags &= !(ffi::HEVC_CHROMA_FORMAT_MASK | ffi::HEVC_BIT_DEPTH_MASK);
    hevc.flags |=
        ffi::HEVC_REPEAT_SPS_PPS | (ffi::CHROMA_FORMAT_420 << ffi::HEVC_CHROMA_FORMAT_SHIFT);
    hevc.idr_period = GOP_FRAMES;
    hevc.min_cu_size = ffi::HEVC_CU_SIZE_16;
    hevc.max_cu_size = ffi::HEVC_CU_SIZE_32;
    hevc.vui.signal_colour(matrix);
}

/// Makes AV1 8-bit 4:2:0 with limited range, BT.709 primaries and transfer and `matrix`
/// signalled, a key frame on every keyframe and the sequence header repeated before each, held
/// to 16×16 to 32×32 partitions, which keeps the encoder fast. The stream is plain low-overhead OBUs, without Annex B framing or the optional
/// timing, decoder model and frame id fields.
fn configure_av1(av1: &mut ffi::Av1Config, matrix: u32) {
    av1.flags &= !(ffi::AV1_ANNEX_B
        | ffi::AV1_TIMING_INFO
        | ffi::AV1_DECODER_MODEL_INFO
        | ffi::AV1_FRAME_ID_NUMBERS
        | ffi::AV1_DISABLE_SEQUENCE_HEADER
        | ffi::AV1_CHROMA_FORMAT_MASK
        | ffi::AV1_BIT_DEPTH_MASK);
    av1.flags |=
        ffi::AV1_REPEAT_SEQUENCE_HEADER | (ffi::CHROMA_FORMAT_420 << ffi::AV1_CHROMA_FORMAT_SHIFT);
    av1.idr_period = GOP_FRAMES;
    av1.min_part_size = ffi::AV1_PART_SIZE_16;
    av1.max_part_size = ffi::AV1_PART_SIZE_32;
    av1.signal_colour(matrix);
}

/// The session parameters for encoding `settings` with `config`, which they point to and which
/// must therefore stay in place until the session is initialized. The zeroed flags leave
/// weighted prediction off: like lookahead, it is a compute pass on the shader cores.
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

/// Creates a BGRA texture of the encoded size for the capture to draw a frame into.
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
    texture.ok_or_else(|| texture_failed(windows::core::Error::empty()))
}

/// Bytes of an output buffer in video memory for frames of `settings`' size: twice the
/// uncompressed picture, as NVIDIA's sample sizes it, plus the header NVENC writes in front.
/// Never less than the ring reads back from it for a frame's head, whatever the picture's size.
fn staging_bytes(settings: Settings) -> usize {
    let picture = settings.width as usize * settings.height as usize * 4;
    (2 * picture).max(super::cuda_output::HEAD_BYTES) + ffi::ENCODE_OUT_PARAMS_BYTES
}

/// Creates the auto-reset event NVENC signals when a slot's frame completes.
fn create_event() -> Result<Owned<HANDLE>, NvencError> {
    // SAFETY: an unnamed auto-reset event with default security.
    let event = unsafe { CreateEventW(None, false, false, None) }
        .map_err(|error| NvencError::Windows("create an encoder event", error))?;
    // SAFETY: the event was just created, and only this `Owned` closes it.
    Ok(unsafe { Owned::new(event) })
}

/// The error for `call` failing with `status`; a rejected structure version means the driver
/// is too old for these structures, whatever the call.
fn call_error(call: &'static str, status: Status, detail: String) -> NvencError {
    if status == Status::INVALID_VERSION {
        return NvencError::VersionRejected;
    }
    NvencError::Call {
        call,
        status,
        detail,
    }
}
