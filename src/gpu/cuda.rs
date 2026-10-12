//! The CUDA driver API (`nvcuda.dll`), loaded at run time: the device, its primary context and device memory.

use std::{
    ffi::{c_char, c_void},
    fmt,
    ptr::{self, NonNull},
};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID},
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    },
    core::{Owned, PCSTR, w},
};

#[derive(Debug, Error)]
pub(crate) enum CudaError {
    #[error("nvcuda.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error("nvcuda.dll has no {0}; update the NVIDIA driver")]
    MissingEntryPoint(&'static str),
    #[error("{0} failed with {1}")]
    Call(&'static str, Status),
}

/// `CUresult`, kept raw: a driver may return codes this program does not know.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Status(i32);

impl Status {
    pub(super) const SUCCESS: Self = Self(0);
    const UNKNOWN: Self = Self(999);

    fn ok(self, call: &'static str) -> Result<(), CudaError> {
        if self == Self::SUCCESS {
            Ok(())
        } else {
            Err(CudaError::Call(call, self))
        }
    }

    const fn name(self) -> &'static str {
        match self.0 {
            0 => "CUDA_SUCCESS",
            1 => "CUDA_ERROR_INVALID_VALUE",
            2 => "CUDA_ERROR_OUT_OF_MEMORY",
            3 => "CUDA_ERROR_NOT_INITIALIZED",
            4 => "CUDA_ERROR_DEINITIALIZED",
            100 => "CUDA_ERROR_NO_DEVICE",
            101 => "CUDA_ERROR_INVALID_DEVICE",
            201 => "CUDA_ERROR_INVALID_CONTEXT",
            202 => "CUDA_ERROR_CONTEXT_ALREADY_CURRENT",
            216 => "CUDA_ERROR_UNSUPPORTED_LIMIT",
            217 => "CUDA_ERROR_CONTEXT_ALREADY_IN_USE",
            304 => "CUDA_ERROR_OPERATING_SYSTEM",
            400 => "CUDA_ERROR_INVALID_HANDLE",
            700 => "CUDA_ERROR_ILLEGAL_ADDRESS",
            719 => "CUDA_ERROR_LAUNCH_FAILED",
            999 => "CUDA_ERROR_UNKNOWN",
            _ => "unknown CUDA status",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name(), self.0)
    }
}

/// `CUdevice`.
pub(super) type Device = i32;

/// `CUdeviceptr`.
pub(super) type DevicePointer = u64;

/// `CUcontext`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Context(NonNull<c_void>);

// SAFETY: a context handle may be made current and used from any thread.
unsafe impl Send for Context {}
// SAFETY: as above; the handle is only ever read.
unsafe impl Sync for Context {}

impl Context {
    pub(super) fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr()
    }
}

const LUID_BYTES: usize = 8;

// Signatures from `cuda.h`; the `_v2` names are what the plain ones resolve to.
type Init = unsafe extern "system" fn(u32) -> Status;
type DriverGetVersion = unsafe extern "system" fn(*mut i32) -> Status;
type DeviceGetCount = unsafe extern "system" fn(*mut i32) -> Status;
type DeviceGet = unsafe extern "system" fn(*mut Device, i32) -> Status;
type DeviceGetLuid = unsafe extern "system" fn(*mut c_char, *mut u32, Device) -> Status;
type DevicePrimaryCtxRetain = unsafe extern "system" fn(*mut *mut c_void, Device) -> Status;
type DevicePrimaryCtxRelease = unsafe extern "system" fn(Device) -> Status;
type CtxGetCurrent = unsafe extern "system" fn(*mut *mut c_void) -> Status;
type CtxPushCurrent = unsafe extern "system" fn(*mut c_void) -> Status;
type CtxPopCurrent = unsafe extern "system" fn(*mut *mut c_void) -> Status;
type MemAlloc = unsafe extern "system" fn(*mut DevicePointer, usize) -> Status;
type MemFree = unsafe extern "system" fn(DevicePointer) -> Status;
type MemGetInfo = unsafe extern "system" fn(*mut usize, *mut usize) -> Status;
type MemcpyDtoH = unsafe extern "system" fn(*mut c_void, DevicePointer, usize) -> Status;
type MemcpyDtoD = unsafe extern "system" fn(DevicePointer, DevicePointer, usize) -> Status;
type StreamSynchronize = unsafe extern "system" fn(*mut c_void) -> Status;
type CtxSynchronize = unsafe extern "system" fn() -> Status;
type CtxSetFlags = unsafe extern "system" fn(u32) -> Status;

pub(super) struct Driver {
    init: Init,
    get_version: DriverGetVersion,
    device_get_count: DeviceGetCount,
    device_get: DeviceGet,
    device_get_luid: DeviceGetLuid,
    device_primary_ctx_retain: DevicePrimaryCtxRetain,
    device_primary_ctx_release: DevicePrimaryCtxRelease,
    ctx_get_current: CtxGetCurrent,
    ctx_push_current: CtxPushCurrent,
    ctx_pop_current: CtxPopCurrent,
    mem_alloc: MemAlloc,
    mem_free: MemFree,
    mem_get_info: MemGetInfo,
    memcpy_dtoh: MemcpyDtoH,
    memcpy_dtod: MemcpyDtoD,
    stream_synchronize: StreamSynchronize,
    ctx_synchronize: CtxSynchronize,
    ctx_set_flags: CtxSetFlags,
    /// Declared last so it is unloaded last.
    _library: Owned<HMODULE>,
}

// SAFETY: the library stays loaded while this lives, and the driver API is thread-safe.
unsafe impl Send for Driver {}
// SAFETY: as above; the entry points are plain function pointers that are only read.
unsafe impl Sync for Driver {}

macro_rules! entry_point {
    ($library:expr, $name:literal, $type:ty) => {{
        // SAFETY: looks a symbol up in the loaded library.
        let address = unsafe { GetProcAddress($library, PCSTR(concat!($name, "\0").as_ptr())) }
            .ok_or(CudaError::MissingEntryPoint($name))?;
        // SAFETY: the export has the declared signature, as the macro's contract says.
        unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $type>(address) }
    }};
}

impl Driver {
    pub(super) fn load() -> Result<Self, CudaError> {
        // System32 only, never a planted copy.
        // SAFETY: loads a system library by name.
        let library =
            unsafe { LoadLibraryExW(w!("nvcuda.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .map_err(|_| CudaError::Library)?;
        // SAFETY: the module was just loaded, and only this `Owned` frees it.
        let library = unsafe { Owned::new(library) };
        let module = *library;
        let driver = Self {
            init: entry_point!(module, "cuInit", Init),
            get_version: entry_point!(module, "cuDriverGetVersion", DriverGetVersion),
            device_get_count: entry_point!(module, "cuDeviceGetCount", DeviceGetCount),
            device_get: entry_point!(module, "cuDeviceGet", DeviceGet),
            device_get_luid: entry_point!(module, "cuDeviceGetLuid", DeviceGetLuid),
            device_primary_ctx_retain: entry_point!(
                module,
                "cuDevicePrimaryCtxRetain",
                DevicePrimaryCtxRetain
            ),
            device_primary_ctx_release: entry_point!(
                module,
                "cuDevicePrimaryCtxRelease",
                DevicePrimaryCtxRelease
            ),
            ctx_get_current: entry_point!(module, "cuCtxGetCurrent", CtxGetCurrent),
            ctx_push_current: entry_point!(module, "cuCtxPushCurrent_v2", CtxPushCurrent),
            ctx_pop_current: entry_point!(module, "cuCtxPopCurrent_v2", CtxPopCurrent),
            mem_alloc: entry_point!(module, "cuMemAlloc_v2", MemAlloc),
            mem_free: entry_point!(module, "cuMemFree_v2", MemFree),
            mem_get_info: entry_point!(module, "cuMemGetInfo_v2", MemGetInfo),
            memcpy_dtoh: entry_point!(module, "cuMemcpyDtoH_v2", MemcpyDtoH),
            memcpy_dtod: entry_point!(module, "cuMemcpyDtoD_v2", MemcpyDtoD),
            stream_synchronize: entry_point!(module, "cuStreamSynchronize", StreamSynchronize),
            ctx_synchronize: entry_point!(module, "cuCtxSynchronize", CtxSynchronize),
            ctx_set_flags: entry_point!(module, "cuCtxSetFlags", CtxSetFlags),
            _library: library,
        };
        // SAFETY: the flags must be 0.
        unsafe { (driver.init)(0) }.ok("cuInit")?;
        Ok(driver)
    }

    pub(super) fn version(&self) -> Result<i32, CudaError> {
        let mut version = 0;
        // SAFETY: writes one integer.
        unsafe { (self.get_version)(&raw mut version) }.ok("cuDriverGetVersion")?;
        Ok(version)
    }

    pub(super) fn device_with_luid(&self, luid: LUID) -> Result<Option<Device>, CudaError> {
        let wanted = luid_bytes(luid);
        let mut count = 0;
        // SAFETY: writes one integer.
        unsafe { (self.device_get_count)(&raw mut count) }.ok("cuDeviceGetCount")?;
        for ordinal in 0..count {
            let mut device = 0;
            // SAFETY: the ordinal is below the count; the out-parameter is written on success.
            unsafe { (self.device_get)(&raw mut device, ordinal) }.ok("cuDeviceGet")?;
            let mut bytes = [0 as c_char; LUID_BYTES];
            let mut node_mask = 0;
            // SAFETY: the buffer holds the eight bytes written.
            unsafe { (self.device_get_luid)(bytes.as_mut_ptr(), &raw mut node_mask, device) }
                .ok("cuDeviceGetLuid")?;
            if bytes.map(i8::cast_unsigned) == wanted {
                return Ok(Some(device));
            }
        }
        Ok(None)
    }

    pub(super) fn first_device(&self) -> Result<Device, CudaError> {
        let mut device = 0;
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.device_get)(&raw mut device, 0) }.ok("cuDeviceGet")?;
        Ok(device)
    }

    pub(super) fn retain_primary_context(&self, device: Device) -> Result<Context, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.device_primary_ctx_retain)(&raw mut context, device) }
            .ok("cuDevicePrimaryCtxRetain")?;
        NonNull::new(context)
            .map(Context)
            .ok_or(CudaError::Call("cuDevicePrimaryCtxRetain", Status::UNKNOWN))
    }

    pub(super) fn release_primary_context(&self, device: Device) -> Result<(), CudaError> {
        // SAFETY: undoes a `retain_primary_context` on the same device.
        unsafe { (self.device_primary_ctx_release)(device) }.ok("cuDevicePrimaryCtxRelease")
    }

    pub(super) fn current_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success, null when none is current.
        unsafe { (self.ctx_get_current)(&raw mut context) }.ok("cuCtxGetCurrent")?;
        Ok(NonNull::new(context).map(Context))
    }

    pub(super) fn push_context(&self, context: Context) -> Result<(), CudaError> {
        // SAFETY: a live context.
        unsafe { (self.ctx_push_current)(context.as_ptr()) }.ok("cuCtxPushCurrent")
    }

    pub(super) fn pop_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.ctx_pop_current)(&raw mut context) }.ok("cuCtxPopCurrent")?;
        Ok(NonNull::new(context).map(Context))
    }

    pub(super) fn allocate(&self, bytes: usize) -> Result<DevicePointer, CudaError> {
        let mut pointer = 0;
        // SAFETY: a context is current; the out-parameter is written on success.
        unsafe { (self.mem_alloc)(&raw mut pointer, bytes) }.ok("cuMemAlloc")?;
        Ok(pointer)
    }

    pub(super) fn free(&self, pointer: DevicePointer) -> Result<(), CudaError> {
        // SAFETY: the pointer came from `allocate` and is freed once.
        unsafe { (self.mem_free)(pointer) }.ok("cuMemFree")
    }

    pub(super) fn memory_info(&self) -> Result<(usize, usize), CudaError> {
        let (mut free, mut total) = (0, 0);
        // SAFETY: a context is current; both out-parameters are written on success.
        unsafe { (self.mem_get_info)(&raw mut free, &raw mut total) }.ok("cuMemGetInfo")?;
        Ok((free, total))
    }

    pub(super) fn copy_to_host(
        &self,
        into: &mut [u8],
        from: DevicePointer,
    ) -> Result<(), CudaError> {
        // SAFETY: the host slice is writable for the length passed.
        unsafe { (self.memcpy_dtoh)(into.as_mut_ptr().cast(), from, into.len()) }.ok("cuMemcpyDtoH")
    }

    pub(super) fn copy_on_device(
        &self,
        to: DevicePointer,
        from: DevicePointer,
        bytes: usize,
    ) -> Result<(), CudaError> {
        // SAFETY: device memory only; the driver rejects an unallocated range.
        unsafe { (self.memcpy_dtod)(to, from, bytes) }.ok("cuMemcpyDtoD")
    }

    pub(super) fn synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: the null stream is the context's default queue.
        unsafe { (self.stream_synchronize)(ptr::null_mut()) }.ok("cuStreamSynchronize")
    }

    /// `CU_CTX_SCHED_BLOCKING_SYNC`: synchronising calls block instead of spinning.
    pub(super) fn block_on_synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the flag is a documented scheduling flag.
        unsafe { (self.ctx_set_flags)(0x04) }.ok("cuCtxSetFlags")
    }

    pub(super) fn synchronize_context(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the call takes no arguments.
        unsafe { (self.ctx_synchronize)() }.ok("cuCtxSynchronize")
    }
}

fn luid_bytes(luid: LUID) -> [u8; LUID_BYTES] {
    let mut bytes = [0; LUID_BYTES];
    bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_named_with_their_codes() {
        assert_eq!(Status::SUCCESS.to_string(), "CUDA_SUCCESS (0)");
        assert_eq!(Status(100).to_string(), "CUDA_ERROR_NO_DEVICE (100)");
        assert_eq!(Status(201).to_string(), "CUDA_ERROR_INVALID_CONTEXT (201)");
        assert_eq!(Status(12345).to_string(), "unknown CUDA status (12345)");
        assert!(Status::SUCCESS.ok("cuInit").is_ok());
        assert_eq!(
            Status(3).ok("cuDeviceGet").unwrap_err().to_string(),
            "cuDeviceGet failed with CUDA_ERROR_NOT_INITIALIZED (3)"
        );
    }

    #[test]
    fn a_luid_is_laid_out_low_part_first() {
        let luid = LUID {
            LowPart: 0x0403_0201,
            HighPart: 0x0807_0605,
        };
        assert_eq!(luid_bytes(luid), [1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
