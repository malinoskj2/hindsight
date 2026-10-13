//! The CUDA driver API (`nvcuda.dll`), loaded at run time: the device, its primary context and device memory.
//!
//! The `_v2` entry points are the 64-bit `CUdeviceptr` and context-stack revisions of CUDA 3.2
//! and 4.0, `cuDevicePrimaryCtxRetain` is from CUDA 7.0 and `cuDeviceGetLuid` from 10.0. The
//! newest entry point used, `cuCtxSetFlags`, is believed to have come with CUDA 12.1 (driver
//! 530 or newer), so that is believed to be the floor; an older driver fails [`Driver::load`]
//! with [`CudaError::MissingEntryPoint`] rather than misbehaving.

use crate::gpu::ffi::{Library, raw_status};
use std::{
    ffi::{c_char, c_void},
    ptr::{self, NonNull},
};
use thiserror::Error;
use windows::{
    Win32::Foundation::LUID,
    core::{PCSTR, w},
};

#[derive(Debug, Error)]
pub(super) enum CudaError {
    #[error("nvcuda.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error("nvcuda.dll has no {0}; update the NVIDIA driver")]
    MissingEntryPoint(&'static str),
    #[error("{0} failed with {1}")]
    Call(&'static str, Status),
}

raw_status! {
    /// `CUresult`.
    pub(super) struct Status, "unknown CUDA status" {
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
    }
}

impl Status {
    const UNKNOWN: Self = Self(999);
}

fn check(call: &'static str, status: Status) -> Result<(), CudaError> {
    status.ok().map_err(|status| CudaError::Call(call, status))
}

/// `CU_CTX_SCHED_BLOCKING_SYNC`.
const CTX_SCHED_BLOCKING_SYNC: u32 = 0x04;

/// `CUdevice`.
pub(super) type Device = i32;

/// `CUdeviceptr`: an address in device memory, not a pointer into this process.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DevicePointer(u64);

impl DevicePointer {
    pub(super) fn offset(self, bytes: usize) -> Self {
        Self(self.0 + bytes as u64)
    }

    /// For the APIs that type a device address as a host pointer.
    pub(super) fn as_api_pointer(self) -> *mut c_void {
        ptr::without_provenance_mut(
            usize::try_from(self.0).expect("a device address fits a 64-bit pointer"),
        )
    }
}

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

/// The driver's entry points, which may be called from any thread.
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
    _library: Library,
}

macro_rules! entry_point {
    ($library:expr, $name:literal, $type:ty) => {{
        // SAFETY: the export has the declared signature, as the macro's contract says.
        let symbol = unsafe { $library.export::<$type>(PCSTR(concat!($name, "\0").as_ptr())) }
            .ok_or(CudaError::MissingEntryPoint($name))?;
        // Detached from the borrow: `Driver` keeps the library in its last field.
        *symbol
    }};
}

impl Driver {
    pub(super) fn load() -> Result<Self, CudaError> {
        let library = Library::load(w!("nvcuda.dll")).map_err(|_| CudaError::Library)?;
        let driver = Self {
            init: entry_point!(library, "cuInit", Init),
            get_version: entry_point!(library, "cuDriverGetVersion", DriverGetVersion),
            device_get_count: entry_point!(library, "cuDeviceGetCount", DeviceGetCount),
            device_get: entry_point!(library, "cuDeviceGet", DeviceGet),
            device_get_luid: entry_point!(library, "cuDeviceGetLuid", DeviceGetLuid),
            device_primary_ctx_retain: entry_point!(
                library,
                "cuDevicePrimaryCtxRetain",
                DevicePrimaryCtxRetain
            ),
            device_primary_ctx_release: entry_point!(
                library,
                "cuDevicePrimaryCtxRelease",
                DevicePrimaryCtxRelease
            ),
            ctx_get_current: entry_point!(library, "cuCtxGetCurrent", CtxGetCurrent),
            ctx_push_current: entry_point!(library, "cuCtxPushCurrent_v2", CtxPushCurrent),
            ctx_pop_current: entry_point!(library, "cuCtxPopCurrent_v2", CtxPopCurrent),
            mem_alloc: entry_point!(library, "cuMemAlloc_v2", MemAlloc),
            mem_free: entry_point!(library, "cuMemFree_v2", MemFree),
            mem_get_info: entry_point!(library, "cuMemGetInfo_v2", MemGetInfo),
            memcpy_dtoh: entry_point!(library, "cuMemcpyDtoH_v2", MemcpyDtoH),
            memcpy_dtod: entry_point!(library, "cuMemcpyDtoD_v2", MemcpyDtoD),
            stream_synchronize: entry_point!(library, "cuStreamSynchronize", StreamSynchronize),
            ctx_synchronize: entry_point!(library, "cuCtxSynchronize", CtxSynchronize),
            ctx_set_flags: entry_point!(library, "cuCtxSetFlags", CtxSetFlags),
            _library: library,
        };
        // SAFETY: the flags must be 0.
        check("cuInit", unsafe { (driver.init)(0) })?;
        Ok(driver)
    }

    pub(super) fn version(&self) -> Result<i32, CudaError> {
        let mut version = 0;
        // SAFETY: writes one integer.
        check("cuDriverGetVersion", unsafe {
            (self.get_version)(&raw mut version)
        })?;
        Ok(version)
    }

    pub(super) fn device_with_luid(&self, luid: LUID) -> Result<Option<Device>, CudaError> {
        let wanted = luid_bytes(luid);
        let mut count = 0;
        // SAFETY: writes one integer.
        check("cuDeviceGetCount", unsafe {
            (self.device_get_count)(&raw mut count)
        })?;
        for ordinal in 0..count {
            let mut device = 0;
            // SAFETY: the ordinal is below the count; the out-parameter is written on success.
            check("cuDeviceGet", unsafe {
                (self.device_get)(&raw mut device, ordinal)
            })?;
            let mut bytes = [0 as c_char; LUID_BYTES];
            let mut node_mask = 0;
            // SAFETY: the buffer holds the eight bytes written.
            check("cuDeviceGetLuid", unsafe {
                (self.device_get_luid)(bytes.as_mut_ptr(), &raw mut node_mask, device)
            })?;
            if bytes.map(i8::cast_unsigned) == wanted {
                return Ok(Some(device));
            }
        }
        Ok(None)
    }

    pub(super) fn first_device(&self) -> Result<Device, CudaError> {
        let mut device = 0;
        // SAFETY: the out-parameter is written on success.
        check("cuDeviceGet", unsafe {
            (self.device_get)(&raw mut device, 0)
        })?;
        Ok(device)
    }

    pub(super) fn retain_primary_context(&self, device: Device) -> Result<Context, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        check("cuDevicePrimaryCtxRetain", unsafe {
            (self.device_primary_ctx_retain)(&raw mut context, device)
        })?;
        NonNull::new(context)
            .map(Context)
            .ok_or(CudaError::Call("cuDevicePrimaryCtxRetain", Status::UNKNOWN))
    }

    pub(super) fn release_primary_context(&self, device: Device) -> Result<(), CudaError> {
        // SAFETY: undoes a `retain_primary_context` on the same device.
        check("cuDevicePrimaryCtxRelease", unsafe {
            (self.device_primary_ctx_release)(device)
        })
    }

    pub(super) fn current_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success, null when none is current.
        check("cuCtxGetCurrent", unsafe {
            (self.ctx_get_current)(&raw mut context)
        })?;
        Ok(NonNull::new(context).map(Context))
    }

    pub(super) fn push_context(&self, context: Context) -> Result<(), CudaError> {
        // SAFETY: a live context.
        check("cuCtxPushCurrent", unsafe {
            (self.ctx_push_current)(context.as_ptr())
        })
    }

    pub(super) fn pop_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        check("cuCtxPopCurrent", unsafe {
            (self.ctx_pop_current)(&raw mut context)
        })?;
        Ok(NonNull::new(context).map(Context))
    }

    pub(super) fn allocate(&self, bytes: usize) -> Result<DevicePointer, CudaError> {
        let mut pointer = DevicePointer(0);
        // SAFETY: a context is current; the out-parameter is written on success.
        check("cuMemAlloc", unsafe {
            (self.mem_alloc)(&raw mut pointer, bytes)
        })?;
        Ok(pointer)
    }

    pub(super) fn free(&self, pointer: DevicePointer) -> Result<(), CudaError> {
        // SAFETY: the pointer came from `allocate` and is freed once.
        check("cuMemFree", unsafe { (self.mem_free)(pointer) })
    }

    pub(super) fn memory_info(&self) -> Result<(usize, usize), CudaError> {
        let (mut free, mut total) = (0, 0);
        // SAFETY: a context is current; both out-parameters are written on success.
        check("cuMemGetInfo", unsafe {
            (self.mem_get_info)(&raw mut free, &raw mut total)
        })?;
        Ok((free, total))
    }

    pub(super) fn copy_to_host(
        &self,
        into: &mut [u8],
        from: DevicePointer,
    ) -> Result<(), CudaError> {
        // SAFETY: the host slice is writable for the length passed.
        check("cuMemcpyDtoH", unsafe {
            (self.memcpy_dtoh)(into.as_mut_ptr().cast(), from, into.len())
        })
    }

    pub(super) fn copy_on_device(
        &self,
        to: DevicePointer,
        from: DevicePointer,
        bytes: usize,
    ) -> Result<(), CudaError> {
        // SAFETY: device memory only; the driver rejects an unallocated range.
        check("cuMemcpyDtoD", unsafe {
            (self.memcpy_dtod)(to, from, bytes)
        })
    }

    pub(super) fn synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: the null stream is the context's default queue.
        check("cuStreamSynchronize", unsafe {
            (self.stream_synchronize)(ptr::null_mut())
        })
    }

    /// Synchronising calls block instead of spinning.
    pub(super) fn block_on_synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the flag is a documented scheduling flag.
        check("cuCtxSetFlags", unsafe {
            (self.ctx_set_flags)(CTX_SCHED_BLOCKING_SYNC)
        })
    }

    pub(super) fn synchronize_context(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the call takes no arguments.
        check("cuCtxSynchronize", unsafe { (self.ctx_synchronize)() })
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
        assert!(check("cuInit", Status::SUCCESS).is_ok());
        assert_eq!(
            check("cuDeviceGet", Status(3)).unwrap_err().to_string(),
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
