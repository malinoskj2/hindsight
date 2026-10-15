//! The NVENC session: the function table, resolved once, and the calls into it, which nothing
//! else in the encoder makes.

use super::{NvencError, Status, config::codec_guid, ffi};
use crate::{
    config::Codec,
    gpu::{
        cuda::ContextHold,
        ffi::{Library, Symbol},
    },
};
use bytemuck::Zeroable;
use std::{
    ffi::{CStr, c_void},
    ptr,
    sync::Arc,
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::{Direct3D9::IDirect3DDevice9Ex, Direct3D11::ID3D11Device},
    },
    core::{GUID, Interface, s, w},
};

/// An open encode session, the device it runs on and the library it lives in. `Drop` destroys
/// the session. Its methods are the only calls into NVENC.
pub(super) struct Session {
    pub(super) api: Api,
    pub(super) handle: ffi::Handle,
    /// Owned so that it outlives the session: `Drop` destroys the handle before the fields are
    /// dropped.
    pub(super) device: SessionDevice,
    /// Declared last so it is freed last.
    _library: Library,
}

/// The device or context an encode session runs on, held by a reference of its own.
pub(super) enum SessionDevice {
    Direct3D11(ID3D11Device),
    Direct3D9(IDirect3DDevice9Ex),
    Cuda(Arc<ContextHold>),
    /// The encode cost measurement in `d3d12_cost` only.
    #[cfg(test)]
    Direct3D12(windows::Win32::Graphics::Direct3D12::ID3D12Device),
}

impl SessionDevice {
    /// The device type and pointer `nvEncOpenEncodeSessionEx` takes. The pointer is valid while
    /// `self` lives.
    fn raw(&self) -> (u32, *mut c_void) {
        match self {
            Self::Direct3D11(device) => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
            Self::Direct3D9(device) => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
            Self::Cuda(hold) => (ffi::DEVICE_TYPE_CUDA, hold.context().as_ptr()),
            #[cfg(test)]
            Self::Direct3D12(device) => (ffi::DEVICE_TYPE_DIRECTX, device.as_raw()),
        }
    }
}

/// The entry points of the function list this program calls, each checked present once, when
/// the session opens.
pub(super) struct Api {
    pub(super) open_encode_session_ex: ffi::OpenEncodeSessionEx,
    pub(super) get_encode_guid_count: ffi::GetEncodeGuidCount,
    pub(super) get_encode_guids: ffi::GetEncodeGuids,
    pub(super) get_encode_caps: ffi::GetEncodeCaps,
    pub(super) get_encode_preset_config_ex: ffi::GetEncodePresetConfigEx,
    pub(super) initialize_encoder: ffi::InitializeEncoder,
    pub(super) create_bitstream_buffer: ffi::CreateBitstreamBufferFn,
    pub(super) destroy_bitstream_buffer: ffi::WithPointer,
    pub(super) encode_picture: ffi::EncodePicture,
    pub(super) lock_bitstream: ffi::LockBitstreamFn,
    pub(super) unlock_bitstream: ffi::WithPointer,
    pub(super) register_async_event: ffi::EventFn,
    pub(super) unregister_async_event: ffi::EventFn,
    pub(super) map_input_resource: ffi::MapInputResourceFn,
    pub(super) unmap_input_resource: ffi::WithPointer,
    pub(super) register_resource: ffi::RegisterResourceFn,
    pub(super) unregister_resource: ffi::WithPointer,
    pub(super) destroy_encoder: ffi::DestroyEncoder,
    pub(super) get_last_error_string: ffi::GetLastErrorString,
}

impl Api {
    fn resolve(list: &ffi::FunctionList) -> Result<Self, NvencError> {
        Ok(Self {
            open_encode_session_ex: entry_point(
                list.open_encode_session_ex,
                "nvEncOpenEncodeSessionEx",
            )?,
            get_encode_guid_count: entry_point(
                list.get_encode_guid_count,
                "nvEncGetEncodeGUIDCount",
            )?,
            get_encode_guids: entry_point(list.get_encode_guids, "nvEncGetEncodeGUIDs")?,
            get_encode_caps: entry_point(list.get_encode_caps, "nvEncGetEncodeCaps")?,
            get_encode_preset_config_ex: entry_point(
                list.get_encode_preset_config_ex,
                "nvEncGetEncodePresetConfigEx",
            )?,
            initialize_encoder: entry_point(list.initialize_encoder, "nvEncInitializeEncoder")?,
            create_bitstream_buffer: entry_point(
                list.create_bitstream_buffer,
                "nvEncCreateBitstreamBuffer",
            )?,
            destroy_bitstream_buffer: entry_point(
                list.destroy_bitstream_buffer,
                "nvEncDestroyBitstreamBuffer",
            )?,
            encode_picture: entry_point(list.encode_picture, "nvEncEncodePicture")?,
            lock_bitstream: entry_point(list.lock_bitstream, "nvEncLockBitstream")?,
            unlock_bitstream: entry_point(list.unlock_bitstream, "nvEncUnlockBitstream")?,
            register_async_event: entry_point(
                list.register_async_event,
                "nvEncRegisterAsyncEvent",
            )?,
            unregister_async_event: entry_point(
                list.unregister_async_event,
                "nvEncUnregisterAsyncEvent",
            )?,
            map_input_resource: entry_point(list.map_input_resource, "nvEncMapInputResource")?,
            unmap_input_resource: entry_point(
                list.unmap_input_resource,
                "nvEncUnmapInputResource",
            )?,
            register_resource: entry_point(list.register_resource, "nvEncRegisterResource")?,
            unregister_resource: entry_point(list.unregister_resource, "nvEncUnregisterResource")?,
            destroy_encoder: entry_point(list.destroy_encoder, "nvEncDestroyEncoder")?,
            get_last_error_string: entry_point(
                list.get_last_error_string,
                "nvEncGetLastErrorString",
            )?,
        })
    }
}

impl Session {
    pub(super) fn open(device: SessionDevice) -> Result<Self, NvencError> {
        let library = load_library()?;
        let api = create_api(&library)?;
        let (device_type, raw_device) = device.raw();
        let mut params = ffi::OpenEncodeSessionExParams::zeroed();
        params.version = ffi::OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = device_type;
        params.device = raw_device;
        params.api_version = ffi::API_VERSION;
        let mut session = Self {
            api,
            handle: ptr::null_mut(),
            device,
            _library: library,
        };
        // SAFETY: the session owns the device and its `Drop` destroys the handle before the
        // device is released. A failed open may still return a handle, which `Drop` destroys.
        let status = unsafe {
            (session.api.open_encode_session_ex)(&raw mut params, &raw mut session.handle)
        };
        session.check("nvEncOpenEncodeSessionEx", status)?;
        Ok(session)
    }

    pub(super) fn encode_guids(&self) -> Result<Vec<GUID>, NvencError> {
        let mut count = 0;
        // SAFETY: an open session; writes one integer to a live local.
        self.check("nvEncGetEncodeGUIDCount", unsafe {
            (self.api.get_encode_guid_count)(self.handle, &raw mut count)
        })?;
        let mut guids = vec![GUID::zeroed(); count as usize];
        let mut written = 0;
        // SAFETY: an open session; the array has room for the `count` GUIDs it is told of.
        self.check("nvEncGetEncodeGUIDs", unsafe {
            (self.api.get_encode_guids)(self.handle, guids.as_mut_ptr(), count, &raw mut written)
        })?;
        guids.truncate(written as usize);
        Ok(guids)
    }

    pub(super) fn capability(&self, codec: Codec, capability: u32) -> Result<u32, NvencError> {
        let mut query = ffi::CapsParam::zeroed();
        query.version = ffi::CAPS_PARAM_VER;
        query.caps_to_query = capability;
        let mut value = 0;
        // SAFETY: an open session, a versioned query structure and one integer to write.
        self.check("nvEncGetEncodeCaps", unsafe {
            (self.api.get_encode_caps)(
                self.handle,
                codec_guid(codec),
                &raw mut query,
                &raw mut value,
            )
        })?;
        u32::try_from(value).map_err(|_| NvencError::BadCapability { capability, value })
    }

    pub(super) fn preset_config(&self, codec: Codec) -> Result<ffi::Config, NvencError> {
        let mut preset = ffi::PresetConfig::zeroed();
        preset.version = ffi::PRESET_CONFIG_VER;
        preset.config.version = ffi::CONFIG_VER;
        // SAFETY: an open session and a versioned preset structure the driver fills in.
        self.check("nvEncGetEncodePresetConfigEx", unsafe {
            (self.api.get_encode_preset_config_ex)(
                self.handle,
                codec_guid(codec),
                ffi::PRESET_P4,
                ffi::TUNING_HIGH_QUALITY,
                &raw mut preset,
            )
        })?;
        Ok(preset.config)
    }

    /// Points `params` at `config` for the duration of the call only. The driver copies both.
    pub(super) fn initialize(
        &self,
        mut params: ffi::InitializeParams,
        config: &mut ffi::Config,
    ) -> Result<(), NvencError> {
        params.encode_config = &raw mut *config;
        // SAFETY: an open session; `params` is versioned and the configuration it points to is
        // borrowed across the call.
        self.check("nvEncInitializeEncoder", unsafe {
            (self.api.initialize_encoder)(self.handle, &raw mut params)
        })
    }

    /// Returns the registration.
    ///
    /// # Safety
    ///
    /// `params.resource_to_register` must be a live resource of the kind `params.resource_type`
    /// names, on this session's device, and stay alive until the registration is undone with
    /// [`unregister_resource`](Self::unregister_resource).
    pub(super) unsafe fn register_resource(
        &self,
        params: &mut ffi::RegisterResource,
    ) -> Result<ffi::Registered, NvencError> {
        // SAFETY: an open session; the caller vouches for the resource.
        self.check("nvEncRegisterResource", unsafe {
            (self.api.register_resource)(self.handle, params)
        })?;
        Ok(params.registered_resource)
    }

    /// Errors are ignored: this is teardown.
    ///
    /// # Safety
    ///
    /// `registered` must come from [`register_resource`](Self::register_resource) on this
    /// session, be unmapped, read or written by no frame in flight, and not be used afterwards.
    pub(super) unsafe fn unregister_resource(&self, registered: ffi::Registered) {
        // SAFETY: as the caller vouches.
        let _ = unsafe { (self.api.unregister_resource)(self.handle, registered) };
    }

    /// # Safety
    ///
    /// `registered` must be a live registration of this session's that is not mapped.
    pub(super) unsafe fn map_input(
        &self,
        registered: ffi::Registered,
    ) -> Result<ffi::MapInputResource, NvencError> {
        let mut map = ffi::MapInputResource::zeroed();
        map.version = ffi::MAP_INPUT_RESOURCE_VER;
        map.registered_resource = registered;
        // SAFETY: an open session, a versioned structure, and a registration the caller vouches
        // for.
        self.check("nvEncMapInputResource", unsafe {
            (self.api.map_input_resource)(self.handle, &raw mut map)
        })?;
        Ok(map)
    }

    /// Errors are ignored: an unmap that fails leaves nothing to undo.
    ///
    /// # Safety
    ///
    /// `mapped` must come from [`map_input`](Self::map_input) on this session and not have been
    /// unmapped since, and no frame still in flight may read it.
    pub(super) unsafe fn unmap_input(&self, mapped: ffi::Mapped) {
        // SAFETY: as the caller vouches.
        let _ = unsafe { (self.api.unmap_input_resource)(self.handle, mapped) };
    }

    pub(super) fn create_bitstream(&self) -> Result<ffi::Bitstream, NvencError> {
        let mut buffer = ffi::CreateBitstreamBuffer::zeroed();
        buffer.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
        // SAFETY: an initialized session and a versioned structure for the driver to fill in.
        self.check("nvEncCreateBitstreamBuffer", unsafe {
            (self.api.create_bitstream_buffer)(self.handle, &raw mut buffer)
        })?;
        Ok(buffer.bitstream_buffer)
    }

    /// Errors are ignored: this is teardown.
    ///
    /// # Safety
    ///
    /// `bitstream` must come from [`create_bitstream`](Self::create_bitstream) on this session,
    /// be unlocked and in no frame in flight, and not be used afterwards.
    pub(super) unsafe fn destroy_bitstream(&self, bitstream: ffi::Bitstream) {
        // SAFETY: as the caller vouches.
        let _ = unsafe { (self.api.destroy_bitstream_buffer)(self.handle, bitstream) };
    }

    /// Copies out the frame in `bitstream`, holding the buffer locked only during the copy.
    /// Returns its picture type and bytes.
    ///
    /// # Safety
    ///
    /// `bitstream` must come from [`create_bitstream`](Self::create_bitstream) on this session,
    /// and the frame last queued into it must have completed.
    pub(super) unsafe fn read_bitstream(
        &self,
        bitstream: ffi::Bitstream,
    ) -> Result<(u32, Box<[u8]>), NvencError> {
        let mut lock = ffi::LockBitstream::zeroed();
        lock.version = ffi::LOCK_BITSTREAM_VER;
        lock.output_bitstream = bitstream;
        // The frame has completed, but a lock that is allowed to wait still costs more.
        lock.flags = ffi::LOCK_DO_NOT_WAIT;
        // SAFETY: a completed buffer of this session's, as the caller vouches; unlocked below.
        self.check("nvEncLockBitstream", unsafe {
            (self.api.lock_bitstream)(self.handle, &raw mut lock)
        })?;
        let data: Box<[u8]> = if lock.bitstream_buffer_ptr.is_null() {
            Box::default()
        } else {
            // SAFETY: while locked, `bitstream_size_in_bytes` bytes are readable at the non-null
            // pointer; they are copied out before the unlock.
            unsafe {
                std::slice::from_raw_parts(
                    lock.bitstream_buffer_ptr.cast::<u8>(),
                    lock.bitstream_size_in_bytes as usize,
                )
            }
            .into()
        };
        // SAFETY: unlocks the buffer locked above; `data` is a copy and does not point into it.
        self.check("nvEncUnlockBitstream", unsafe {
            (self.api.unlock_bitstream)(self.handle, bitstream)
        })?;
        Ok((lock.picture_type, data))
    }

    /// # Safety
    ///
    /// `event` must stay open until it is unregistered with
    /// [`unregister_event`](Self::unregister_event).
    pub(super) unsafe fn register_event(&self, event: HANDLE) -> Result<(), NvencError> {
        let mut params = ffi::EventParams::zeroed();
        params.version = ffi::EVENT_PARAMS_VER;
        params.completion_event = event.0;
        // SAFETY: an open session and a versioned structure; the caller keeps the event open.
        self.check("nvEncRegisterAsyncEvent", unsafe {
            (self.api.register_async_event)(self.handle, &raw mut params)
        })
    }

    /// Errors are ignored: this is teardown, and an event never registered is only reported.
    ///
    /// # Safety
    ///
    /// `event` must still be open.
    pub(super) unsafe fn unregister_event(&self, event: HANDLE) {
        let mut params = ffi::EventParams::zeroed();
        params.version = ffi::EVENT_PARAMS_VER;
        params.completion_event = event.0;
        // SAFETY: an open session, a versioned structure and an open event.
        let _ = unsafe { (self.api.unregister_async_event)(self.handle, &raw mut params) };
    }

    /// Queues a frame.
    ///
    /// # Safety
    ///
    /// `picture.input_buffer` must be mapped by this session; `picture.output_bitstream` must
    /// be this session's, unlocked and in no other frame in flight; `picture.completion_event`
    /// must be registered with this session. All must stay so until the frame completes.
    pub(super) unsafe fn encode_picture(
        &self,
        picture: &mut ffi::PicParams,
    ) -> Result<(), NvencError> {
        // SAFETY: an initialized session; the caller vouches for the buffers and event.
        self.check("nvEncEncodePicture", unsafe {
            (self.api.encode_picture)(self.handle, picture)
        })
    }

    /// Flushes the encoder; `event` is signalled once every queued frame is done.
    ///
    /// # Safety
    ///
    /// `event` must be registered with this session and open.
    pub(super) unsafe fn end_of_stream(&self, event: HANDLE) -> Result<(), NvencError> {
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.encode_pic_flags = ffi::PIC_FLAG_EOS;
        picture.completion_event = event.0;
        // SAFETY: an end-of-stream picture carries no buffers; the caller vouches for the event.
        unsafe { self.encode_picture(&mut picture) }
    }

    pub(super) fn check(&self, call: &'static str, status: Status) -> Result<(), NvencError> {
        check(call, status, || self.last_error_detail())
    }

    fn last_error_detail(&self) -> String {
        if self.handle.is_null() {
            return String::new();
        }
        // SAFETY: an open session; returns a NUL-terminated string it owns, or null.
        let text = unsafe { (self.api.get_last_error_string)(self.handle) };
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

    /// Destroys the session. Every buffer and registration of the session must have been
    /// released first.
    pub(super) fn destroy(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: a live handle, used by nothing afterwards; the caller has released every
        // buffer and registration of the session.
        let _ = unsafe { (self.api.destroy_encoder)(self.handle) };
        self.handle = ptr::null_mut();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn load_library() -> Result<Library, NvencError> {
    Library::load(w!("nvEncodeAPI64.dll")).map_err(|_| NvencError::Library)
}

fn create_api(library: &Library) -> Result<Api, NvencError> {
    let (max_version, create_instance) = resolve_exports(library)?;
    require_driver_version(*max_version)?;
    let mut list = Box::new(ffi::FunctionList::zeroed());
    list.version = ffi::FUNCTION_LIST_VER;
    // SAFETY: the list is a properly versioned, writable `NV_ENCODE_API_FUNCTION_LIST`.
    let status = unsafe { create_instance(&raw mut *list) };
    check("NvEncodeAPICreateInstance", status, String::new)?;
    Api::resolve(&list)
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
