//! The CUDA driver API (`nvcuda.dll`, installed with the NVIDIA driver), as far as NvFBC's
//! CUDA interface and NVENC's CUDA input need it: the device, its primary context, and plain
//! device memory for the frames. No CUDA toolkit is involved; the library is loaded at run time
//! like NVENC and NvFBC, and only the handful of entry points below are looked up.
//!
//! The entry points are the ones `cuda.h` resolves the plain names to: `cuMemAlloc` is
//! `cuMemAlloc_v2` (64-bit device pointers), and so on; the names were checked against the
//! loader `FFmpeg` ships for the same library.

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

/// Why a CUDA call could not be made, or failed.
#[derive(Debug, Error)]
pub(crate) enum CudaError {
    #[error("nvcuda.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error("nvcuda.dll has no {0}; update the NVIDIA driver")]
    MissingEntryPoint(&'static str),
    #[error("{0} failed with {1}")]
    Call(&'static str, Status),
}

/// `CUresult`; kept as an integer, since a driver may return values this program does not
/// know.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Status(i32);

impl Status {
    pub(super) const SUCCESS: Self = Self(0);
    const UNKNOWN: Self = Self(999);

    /// `Ok` for `SUCCESS`, and any other status as the error of `call`.
    fn ok(self, call: &'static str) -> Result<(), CudaError> {
        if self == Self::SUCCESS {
            Ok(())
        } else {
            Err(CudaError::Call(call, self))
        }
    }

    /// The header's name for the status, for the codes the calls here can return.
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

/// The name with the raw code, which stays meaningful for codes newer than this program.
impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name(), self.0)
    }
}

/// `CUdevice`: a device ordinal.
pub(super) type Device = i32;

/// `CUdeviceptr`: an address in device memory, which is 64 bits wide on this platform.
pub(super) type DevicePointer = u64;

/// `CUcontext`: a context the driver made, which stays valid until whoever made it destroys
/// or releases it.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Context(NonNull<c_void>);

// SAFETY: a CUDA context is a handle the driver lets any thread make current and use; nothing
// about it is tied to the thread that made it.
unsafe impl Send for Context {}
// SAFETY: as above; the handle is only ever read.
unsafe impl Sync for Context {}

impl Context {
    /// The handle as other NVIDIA APIs take it.
    pub(super) fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr()
    }
}

/// Fits `LUID`'s eight bytes, as `cuDeviceGetLuid` writes them.
const LUID_BYTES: usize = 8;

// The entry points, with the signatures `cuda.h` declares for them.
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

/// The loaded driver library and its entry points. Everything made through it (contexts,
/// memory) must be released before it is dropped, which unloads the library.
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
    /// Declared last, so it is unloaded after the entry points above are no longer reachable.
    _library: Owned<HMODULE>,
}

// SAFETY: the driver library stays loaded while the `Owned` module handle lives, and the CUDA
// driver API is thread-safe: its entry points may be called from any thread, with a context
// made current there.
unsafe impl Send for Driver {}
// SAFETY: as above; the entry points are plain function pointers that are only read.
unsafe impl Sync for Driver {}

/// Looks `name` up in `library` as an entry point of type `T`, which must be the function
/// pointer type `cuda.h` declares for that export.
macro_rules! entry_point {
    ($library:expr, $name:literal, $type:ty) => {{
        // SAFETY: looks a symbol up in the loaded CUDA driver library; the caller names the
        // type `cuda.h` declares for the export.
        let address = unsafe { GetProcAddress($library, PCSTR(concat!($name, "\0").as_ptr())) }
            .ok_or(CudaError::MissingEntryPoint($name))?;
        // SAFETY: the export has the declared signature, as the macro's contract says.
        unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $type>(address) }
    }};
}

impl Driver {
    /// Loads the driver library and initializes the driver API.
    pub(super) fn load() -> Result<Self, CudaError> {
        // The driver installs the library in System32; never look anywhere a planted copy
        // could be.
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

    /// The driver's version as it reports it: 1000 × major + 10 × minor.
    pub(super) fn version(&self) -> Result<i32, CudaError> {
        let mut version = 0;
        // SAFETY: writes one integer.
        unsafe { (self.get_version)(&raw mut version) }.ok("cuDriverGetVersion")?;
        Ok(version)
    }

    /// The device whose adapter has `luid`, if the driver knows one.
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
            // SAFETY: the buffer holds the eight bytes the call writes; both out-parameters
            // outlive the call.
            unsafe { (self.device_get_luid)(bytes.as_mut_ptr(), &raw mut node_mask, device) }
                .ok("cuDeviceGetLuid")?;
            if bytes.map(i8::cast_unsigned) == wanted {
                return Ok(Some(device));
            }
        }
        Ok(None)
    }

    /// The device with ordinal 0, the one CUDA considers first.
    pub(super) fn first_device(&self) -> Result<Device, CudaError> {
        let mut device = 0;
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.device_get)(&raw mut device, 0) }.ok("cuDeviceGet")?;
        Ok(device)
    }

    /// Takes a reference on the primary context of `device`, creating it the first time, and
    /// returns it; every retain is undone with [`release_primary_context`](Self::release_primary_context).
    pub(super) fn retain_primary_context(&self, device: Device) -> Result<Context, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.device_primary_ctx_retain)(&raw mut context, device) }
            .ok("cuDevicePrimaryCtxRetain")?;
        // A driver that reports success without a context is treated as having failed in an
        // unknown way.
        NonNull::new(context)
            .map(Context)
            .ok_or(CudaError::Call("cuDevicePrimaryCtxRetain", Status::UNKNOWN))
    }

    /// Gives back one reference on the primary context of `device`; the context is destroyed
    /// with the last one.
    pub(super) fn release_primary_context(&self, device: Device) -> Result<(), CudaError> {
        // SAFETY: undoes a `retain_primary_context` on the same device.
        unsafe { (self.device_primary_ctx_release)(device) }.ok("cuDevicePrimaryCtxRelease")
    }

    /// The context current on this thread, if any.
    pub(super) fn current_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success, null when none is current.
        unsafe { (self.ctx_get_current)(&raw mut context) }.ok("cuCtxGetCurrent")?;
        Ok(NonNull::new(context).map(Context))
    }

    /// Makes `context` current on this thread, on top of whatever was.
    pub(super) fn push_context(&self, context: Context) -> Result<(), CudaError> {
        // SAFETY: a live context.
        unsafe { (self.ctx_push_current)(context.as_ptr()) }.ok("cuCtxPushCurrent")
    }

    /// Takes the current context off this thread, making the one below it current again, and
    /// returns it.
    pub(super) fn pop_context(&self) -> Result<Option<Context>, CudaError> {
        let mut context = ptr::null_mut();
        // SAFETY: the out-parameter is written on success.
        unsafe { (self.ctx_pop_current)(&raw mut context) }.ok("cuCtxPopCurrent")?;
        Ok(NonNull::new(context).map(Context))
    }

    /// Allocates `bytes` of device memory in the current context, aligned for any use.
    pub(super) fn allocate(&self, bytes: usize) -> Result<DevicePointer, CudaError> {
        let mut pointer = 0;
        // SAFETY: a context is current; the out-parameter is written on success.
        unsafe { (self.mem_alloc)(&raw mut pointer, bytes) }.ok("cuMemAlloc")?;
        Ok(pointer)
    }

    /// Frees memory from [`allocate`](Self::allocate), which nothing may still read.
    pub(super) fn free(&self, pointer: DevicePointer) -> Result<(), CudaError> {
        // SAFETY: the pointer came from `allocate` and is freed once.
        unsafe { (self.mem_free)(pointer) }.ok("cuMemFree")
    }

    /// Bytes of device memory free and in all, for the current context's device.
    pub(super) fn memory_info(&self) -> Result<(usize, usize), CudaError> {
        let (mut free, mut total) = (0, 0);
        // SAFETY: a context is current; both out-parameters are written on success.
        unsafe { (self.mem_get_info)(&raw mut free, &raw mut total) }.ok("cuMemGetInfo")?;
        Ok((free, total))
    }

    /// Copies `into.len()` bytes of device memory at `from` into `into`, waiting for the copy
    /// and for every copy queued before it.
    pub(super) fn copy_to_host(
        &self,
        into: &mut [u8],
        from: DevicePointer,
    ) -> Result<(), CudaError> {
        // SAFETY: the host slice is writable for exactly the length passed; a device range
        // that is not allocated is reported by the driver, not written from.
        unsafe { (self.memcpy_dtoh)(into.as_mut_ptr().cast(), from, into.len()) }.ok("cuMemcpyDtoH")
    }

    /// Queues a copy of `bytes` of device memory from `from` to `to`, both allocated in the
    /// current context, which may still be under way when the call returns.
    pub(super) fn copy_on_device(
        &self,
        to: DevicePointer,
        from: DevicePointer,
        bytes: usize,
    ) -> Result<(), CudaError> {
        // SAFETY: the copy touches device memory only, through the driver, which reports a
        // range that is not allocated instead of touching host memory.
        unsafe { (self.memcpy_dtod)(to, from, bytes) }.ok("cuMemcpyDtoD")
    }

    /// Waits for every copy queued so far in the current context.
    pub(super) fn synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: the null stream is the context's default queue.
        unsafe { (self.stream_synchronize)(ptr::null_mut()) }.ok("cuStreamSynchronize")
    }

    /// Waits for all the work queued in the current context, on every stream, the driver's
    /// own included.
    /// Makes the current context's synchronising calls block the thread rather than spin
    /// (`CU_CTX_SCHED_BLOCKING_SYNC`).
    pub(super) fn block_on_synchronize(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the flag is a documented scheduling flag.
        unsafe { (self.ctx_set_flags)(0x04) }.ok("cuCtxSetFlags")
    }

    pub(super) fn synchronize_context(&self) -> Result<(), CudaError> {
        // SAFETY: a context is current; the call takes no arguments.
        unsafe { (self.ctx_synchronize)() }.ok("cuCtxSynchronize")
    }
}

/// A LUID's eight bytes in memory order, which is how `cuDeviceGetLuid` reports them.
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
