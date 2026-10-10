//! NVIDIA Frame Buffer Capture (NvFBC) of a monitor, the capture Instant Replay itself uses.
//! The driver copies the display, cursor included, straight from the frame buffer it scans
//! out, so it costs a game nothing that Windows composes.
//!
//! NvFBC on Windows writes into Direct3D 9 surfaces. Two are created on a Direct3D 9Ex device
//! on the monitor's GPU, shared with the encoder's Direct3D 11 device: on each tick the grab
//! the previous tick started is copied into the encoder's input texture on the GPU, and the
//! next grab goes into the other surface, so the capture thread never waits for the GPU.
//! Every grab counts as a new frame, since the driver does not say whether the screen changed.
//!
//! The driver only lets a licensed application capture with NvFBC on a GeForce GPU: it must
//! present the key NVIDIA issued, which is entered in Settings and read from the registry. NvFBC
//! must also be turned on in the driver, which takes administrator rights once.
//!
//! Like Desktop Duplication, NvFBC sees whole monitors connected to the GPU, so a window target
//! is recorded through its monitor. When the driver ends the session (a mode change, the
//! secure desktop) it is opened again in place, keeping the buffer, unless the size changed.

mod ffi;

use super::{CaptureError, Frame, REOPEN_INTERVAL, Source, failed, monitor_of, window_closed};
use crate::{gpu::device::Device, log, service::Target, sys::nvfbc_key};
use std::{
    ptr::{self, NonNull},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HANDLE, HMODULE, HWND, LUID, S_OK},
        Graphics::{
            Direct3D9::{
                D3D_SDK_VERSION, D3DCREATE_FPU_PRESERVE, D3DCREATE_HARDWARE_VERTEXPROCESSING,
                D3DCREATE_MULTITHREADED, D3DCREATE_NOWINDOWCHANGES, D3DDEVTYPE_HAL,
                D3DDISPLAYMODEEX, D3DDISPLAYROTATION, D3DDISPLAYROTATION_IDENTITY, D3DFMT_A8R8G8B8,
                D3DFMT_UNKNOWN, D3DGETDATA_FLUSH, D3DISSUE_END, D3DPOOL_DEFAULT,
                D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS, D3DQUERYTYPE_EVENT,
                D3DSWAPEFFECT_DISCARD, D3DUSAGE_RENDERTARGET, Direct3DCreate9Ex, IDirect3D9Ex,
                IDirect3DDevice9Ex, IDirect3DQuery9, IDirect3DSurface9, IDirect3DTexture9,
            },
            Direct3D11::ID3D11Texture2D,
            Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
        },
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        UI::WindowsAndMessaging::GetDesktopWindow,
    },
    core::{BOOL, Interface, s, w},
};

// Re-exported because `CaptureError::Nvfbc` carries it.
pub(in crate::gpu) use ffi::Status;
use ffi::Zeroed;

/// How long a grab may take on the GPU before the driver is judged to have stopped.
const GRAB_TIMEOUT: Duration = Duration::from_secs(2);
/// Surfaces grabbed into in turn.
const SLOT_COUNT: u32 = 2;
const SLOTS: usize = SLOT_COUNT as usize;

/// A Direct3D 9 surface NvFBC grabs into, the same memory opened on the encoder's device, and
/// a query that tells when the grab has finished on the GPU.
struct Slot {
    surface: IDirect3DSurface9,
    /// The texture `surface` is the only level of, kept alive for as long as the surface.
    _texture: IDirect3DTexture9,
    shared: ID3D11Texture2D,
    done: IDirect3DQuery9,
}

/// `NvFBC64.dll`, unloaded when dropped. Entry points looked up in it, and objects it made,
/// must be gone by then.
struct Library(HMODULE);

impl Library {
    fn load() -> Result<Self, CaptureError> {
        // The driver installs the library in System32; never look anywhere a planted copy
        // could be.
        // SAFETY: loads a system library by name.
        unsafe { LoadLibraryExW(w!("NvFBC64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map(Self)
            .map_err(|_| {
                CaptureError::Unsupported(
                    "NvFBC64.dll was not found; install or update the NVIDIA driver".to_owned(),
                )
            })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the library was loaded by `load`, and whoever holds it drops everything that
        // came from it first: `Nvfbc` releases its session in its own `Drop`.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

/// A capture object of the driver's, released when dropped.
struct Session {
    /// Live from `NvFBC_CreateEx` until `Drop` releases it; its method table lives in the
    /// library, which outlives the session.
    object: NonNull<ffi::ToDx9Vid>,
    /// The surfaces handed to the driver at setup, kept for as long as it may look at them.
    buffers: Box<[ffi::OutputBuffer; SLOTS]>,
}

impl Session {
    /// The object's method table.
    fn methods(&self) -> &ffi::ToDx9VidVtable {
        // SAFETY: the object is live (see the field), and a live object's table pointer is
        // valid for as long as the library that made it is loaded.
        unsafe { &*(*self.object.as_ptr()).vtable }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live object from `NvFBC_CreateEx`; nothing uses it afterwards.
        let _ = unsafe { (self.methods().release)(self.object.as_ptr()) };
    }
}

pub(super) struct Nvfbc {
    /// Unloaded only after `Drop` has released the session, the one thing here that needs it.
    _library: Library,
    create: ffi::CreateEx,
    key: Vec<u8>,
    device: IDirect3DDevice9Ex,
    slots: [Slot; SLOTS],
    /// `None` while the driver has taken it away.
    session: Option<Session>,
    /// The slot whose grab is on its way, to be copied on the next tick.
    pending: Option<usize>,
    /// The slot the next grab goes into.
    next: u32,
    size: (u32, u32),
    monitor: HMONITOR,
    /// The window whose monitor is recorded, when the target is one.
    window: Option<HWND>,
    /// When a lost session is tried again.
    reopen_at: Option<Instant>,
}

impl Nvfbc {
    pub(super) fn start(device: &Device, target: &Target) -> Result<Self, CaptureError> {
        let (monitor, window) = monitor_of(target);
        let key = saved_key()?;

        // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
        let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(failed("start Direct3D 9 for NvFBC"))?;
        let adapter = find_adapter(&d3d9, device, monitor)?;
        let size = display_size(&d3d9, adapter)?;

        let library = Library::load()?;
        let create = prepare(&library, &key, adapter)?;
        let d3d9_device = create_device(&d3d9, adapter)?;
        let slots = [
            create_slot(&d3d9_device, device, size)?,
            create_slot(&d3d9_device, device, size)?,
        ];

        let mut capture = Self {
            _library: library,
            create,
            key,
            device: d3d9_device,
            slots,
            session: None,
            pending: None,
            next: 0,
            size,
            monitor,
            window,
            reopen_at: None,
        };
        capture.session = Some(
            capture
                .open_session()
                .map_err(nvfbc_failed("create the NvFBC session"))?,
        );
        Ok(capture)
    }

    /// Creates the driver's capture object on the device and hands it the surfaces.
    fn open_session(&self) -> Result<Session, Status> {
        let mut params = ffi::CreateParams::zeroed();
        params.version = ffi::CREATE_PARAMS_VER;
        params.interface_type = ffi::TO_DX9_VID;
        params.device = self.device.as_raw();
        params.private_data = self.key.as_ptr().cast();
        params.private_data_size = key_size(&self.key);
        // SAFETY: the parameters are complete; the device and the key outlive the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        let object = NonNull::new(params.nvfbc).ok_or(Status::UNSUPPORTED)?;

        let buffers = Box::new(std::array::from_fn(|index| {
            let mut buffer = ffi::OutputBuffer::zeroed();
            buffer.primary = self.slots[index].surface.as_raw();
            buffer
        }));
        // Released by `Drop` should setup fail.
        let session = Session { object, buffers };
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::TODX9VID_SETUP_PARAMS_VER;
        setup.flags = ffi::SETUP_WITH_HW_CURSOR;
        setup.mode = ffi::TODX9VID_ARGB;
        setup.buffer_count = SLOT_COUNT;
        setup.buffers = session.buffers.as_ptr().cast_mut();
        // SAFETY: a live object; the surfaces belong to the device it was created on and live
        // as long as the session.
        unsafe { (session.methods().set_up)(object.as_ptr(), &raw mut setup) }.ok()?;
        Ok(session)
    }

    /// Opens the session again after the driver ended it. While the driver refuses, the last
    /// picture is repeated; a monitor that went away ends the capture.
    fn reopen(&mut self) -> Result<Frame, CaptureError> {
        let now = Instant::now();
        if self.reopen_at.is_some_and(|due| now < due) {
            return Ok(Frame::Unchanged);
        }
        if !is_attached(self.monitor) {
            return Err(CaptureError::Closed);
        }
        match self.open_session() {
            Ok(session) => {
                self.session = Some(session);
                self.reopen_at = None;
            }
            Err(status) => {
                if let Some(error) = permanent(status) {
                    return Err(error);
                }
                self.reopen_at = Some(now + REOPEN_INTERVAL);
            }
        }
        Ok(Frame::Unchanged)
    }

    /// Copies the previous tick's grab into `target` once the GPU has finished it, and
    /// reports whether there was one.
    fn collect(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<bool, CaptureError> {
        let Some(index) = self.pending.take() else {
            return Ok(false);
        };
        let slot = &self.slots[index];
        wait_for(&slot.done)?;
        device.copy_region(target, &slot.shared, width, height);
        Ok(true)
    }

    /// Starts a grab of the display into the next slot.
    fn grab(&mut self) -> Result<Grab, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grab::Lost);
        };
        let mut info = ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::TODX9VID_GRAB_FRAME_PARAMS_VER;
        params.flags = ffi::TODX9VID_NOWAIT;
        params.grab_mode = ffi::TODX9VID_SOURCEMODE_FULL;
        params.buffer_index = self.next;
        params.grab_info = &raw mut info;
        // SAFETY: a live, set-up object; `info` outlives the call.
        let status =
            unsafe { (session.methods().grab_frame)(session.object.as_ptr(), &raw mut params) };
        match status {
            Status::SUCCESS if info.must_recreate != 0 => Ok(Grab::Lost),
            Status::SUCCESS if (info.width, info.height) != self.size => Ok(Grab::Resized),
            Status::SUCCESS => {
                let slot = &self.slots[self.next as usize];
                // SAFETY: marks the end of the grab's work on the device.
                unsafe { slot.done.Issue(D3DISSUE_END) }
                    .map_err(failed("follow the NvFBC grab"))?;
                self.pending = Some(self.next as usize);
                self.next = (self.next + 1) % SLOT_COUNT;
                Ok(Grab::Started)
            }
            Status::INVALIDATED_SESSION => Ok(Grab::Lost),
            // Protected video is on screen; the driver withholds the picture until it is gone.
            Status::PROTECTED_CONTENT => Ok(Grab::Withheld),
            status => Err(nvfbc_failed("grab the display with NvFBC")(status)),
        }
    }
}

/// What starting a grab came to.
enum Grab {
    Started,
    Withheld,
    Lost,
    Resized,
}

impl Source for Nvfbc {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn captures_window(&self) -> bool {
        false
    }

    fn take(
        &mut self,
        device: &Device,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        if window_closed(self.window) {
            return Err(CaptureError::Closed);
        }
        let collected = self.collect(device, target, width, height)?;
        match self.grab()? {
            Grab::Started | Grab::Withheld => {}
            Grab::Resized => return Ok(Frame::Resized),
            Grab::Lost if collected => self.session = None,
            Grab::Lost => {
                self.session = None;
                return self.reopen();
            }
        }
        Ok(if collected {
            Frame::Fresh
        } else {
            Frame::Unchanged
        })
    }
}

impl Drop for Nvfbc {
    fn drop(&mut self) {
        // The driver's object goes before the library that implements it, which is unloaded
        // when the fields are dropped after this.
        self.session = None;
    }
}

/// The key saved in Settings; without one the driver refuses NvFBC on a GeForce GPU.
fn saved_key() -> Result<Vec<u8>, CaptureError> {
    match nvfbc_key::load() {
        Ok(Some(key)) => Ok(key),
        Ok(None) => Err(CaptureError::Unsupported(
            "no NvFBC key has been entered; choose NVIDIA NvFBC in Settings again to enter the key NVIDIA issued you"
                .to_owned(),
        )),
        Err(error) => Err(CaptureError::Unsupported(format!(
            "the NvFBC key could not be read from the registry: {error}"
        ))),
    }
}

fn key_size(key: &[u8]) -> u32 {
    // At most `nvfbc_key::MAX_BYTES`, checked when it was entered.
    u32::try_from(key.len()).unwrap_or(u32::MAX)
}

/// The Direct3D 9 adapter that shows `monitor`, which must be on the encoding GPU for the
/// surfaces to be shared with it.
fn find_adapter(
    d3d9: &IDirect3D9Ex,
    device: &Device,
    monitor: HMONITOR,
) -> Result<u32, CaptureError> {
    let other_gpu = || {
        CaptureError::Unsupported(format!(
            "NvFBC can only capture monitors connected to {}, the GPU that encodes; choose Windows Graphics Capture",
            device.adapter
        ))
    };
    // SAFETY: counts the adapters of a live object.
    let adapter = (0..unsafe { d3d9.GetAdapterCount() })
        // SAFETY: every index below the count names an adapter.
        .find(|&adapter| unsafe { d3d9.GetAdapterMonitor(adapter) } == monitor)
        .ok_or_else(other_gpu)?;
    let mut luid = LUID::default();
    // SAFETY: the adapter exists; the out-parameter is written on success.
    unsafe { d3d9.GetAdapterLUID(adapter, &raw mut luid) }
        .map_err(failed("identify the monitor's GPU"))?;
    let encoding = device.luid().map_err(failed("identify the encoding GPU"))?;
    if luid != encoding {
        return Err(other_gpu());
    }
    Ok(adapter)
}

/// The monitor's current size; a turned monitor is refused, as with Desktop Duplication.
fn display_size(d3d9: &IDirect3D9Ex, adapter: u32) -> Result<(u32, u32), CaptureError> {
    let mut mode = D3DDISPLAYMODEEX {
        Size: u32::try_from(size_of::<D3DDISPLAYMODEEX>()).expect("a display mode is a few bytes"),
        ..Default::default()
    };
    let mut rotation = D3DDISPLAYROTATION::default();
    // SAFETY: the adapter exists; both out-parameters are written on success.
    unsafe { d3d9.GetAdapterDisplayModeEx(adapter, &raw mut mode, &raw mut rotation) }
        .map_err(failed("read the monitor's display mode"))?;
    if rotation != D3DDISPLAYROTATION_IDENTITY {
        return Err(CaptureError::Unsupported(
            "the monitor is rotated, which NvFBC capture does not undo; choose Windows Graphics Capture"
                .to_owned(),
        ));
    }
    Ok((mode.Width, mode.Height))
}

/// Checks that the driver will capture `adapter` with this key, turning NvFBC on in the driver
/// when it is off, and returns the entry point that creates a session.
fn prepare(library: &Library, key: &[u8], adapter: u32) -> Result<ffi::CreateEx, CaptureError> {
    let library = library.0;
    let missing = |name: &str| {
        CaptureError::Unsupported(format!(
            "NvFBC64.dll has no {name}; update the NVIDIA driver"
        ))
    };
    // SAFETY: looks symbols up in the loaded NvFBC library; each has the documented signature.
    let (get_status, create, enable) = unsafe {
        let get_status = GetProcAddress(library, s!("NvFBC_GetStatusEx"))
            .ok_or_else(|| missing("NvFBC_GetStatusEx"))?;
        let create = GetProcAddress(library, s!("NvFBC_CreateEx"))
            .ok_or_else(|| missing("NvFBC_CreateEx"))?;
        let enable =
            GetProcAddress(library, s!("NvFBC_Enable")).ok_or_else(|| missing("NvFBC_Enable"))?;
        (
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::GetStatusEx>(
                get_status,
            ),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::CreateEx>(create),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::Enable>(enable),
        )
    };

    let mut status = ffi::StatusEx::zeroed();
    status.version = ffi::STATUS_VER;
    status.adapter_index = adapter;
    status.private_data = key.as_ptr().cast();
    status.private_data_size = key_size(key);
    // SAFETY: the parameters are complete; the key outlives the call.
    unsafe { get_status(&raw mut status) }
        .ok()
        .map_err(nvfbc_failed("read the NvFBC status"))?;
    if status.flags & ffi::STATUS_CAPTURE_POSSIBLE == 0 {
        // SAFETY: a plain call into the loaded library.
        let result = unsafe { enable(ffi::STATE_ENABLE) };
        if result == Status::INSUFFICIENT_PRIVILEGES {
            return Err(CaptureError::Unsupported(
                "NvFBC is turned off in the NVIDIA driver; run hindsight once as administrator to turn it on"
                    .to_owned(),
            ));
        }
        if result != Status::SUCCESS {
            return Err(CaptureError::Unsupported(format!(
                "NvFBC is turned off in the NVIDIA driver and could not be turned on: {result}"
            )));
        }
        log::info("NvFBC was off in the NVIDIA driver and has been turned on");
    }
    Ok(create)
}

/// A Direct3D 9Ex device on `adapter` that only holds the grab surfaces; it never presents.
fn create_device(d3d9: &IDirect3D9Ex, adapter: u32) -> Result<IDirect3DDevice9Ex, CaptureError> {
    let mut parameters = D3DPRESENT_PARAMETERS {
        BackBufferWidth: 1,
        BackBufferHeight: 1,
        BackBufferFormat: D3DFMT_UNKNOWN,
        BackBufferCount: 1,
        SwapEffect: D3DSWAPEFFECT_DISCARD,
        // SAFETY: returns the desktop's handle; no preconditions.
        hDeviceWindow: unsafe { GetDesktopWindow() },
        Windowed: BOOL::from(true),
        PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        ..Default::default()
    };
    let flags = D3DCREATE_FPU_PRESERVE
        | D3DCREATE_MULTITHREADED
        | D3DCREATE_HARDWARE_VERTEXPROCESSING
        | D3DCREATE_NOWINDOWCHANGES;
    let mut device = None;
    // SAFETY: windowed parameters on the desktop window; the out-parameter is written on
    // success.
    unsafe {
        d3d9.CreateDeviceEx(
            adapter,
            D3DDEVTYPE_HAL,
            HWND::default(),
            flags.cast_unsigned(),
            &raw mut parameters,
            ptr::null_mut(),
            &raw mut device,
        )
    }
    .and_then(|()| device.ok_or_else(windows::core::Error::empty))
    .map_err(failed("create the Direct3D 9 device for NvFBC"))
}

/// A surface the size of the display, shared with the encoder's device.
fn create_slot(
    d3d9: &IDirect3DDevice9Ex,
    device: &Device,
    (width, height): (u32, u32),
) -> Result<Slot, CaptureError> {
    let mut texture = None;
    let mut handle = HANDLE::default();
    // SAFETY: out-parameters are written on success; a shared texture must be in the default
    // pool, as it is.
    let texture: IDirect3DTexture9 = unsafe {
        d3d9.CreateTexture(
            width,
            height,
            1,
            D3DUSAGE_RENDERTARGET.cast_unsigned(),
            D3DFMT_A8R8G8B8,
            D3DPOOL_DEFAULT,
            &raw mut texture,
            &raw mut handle,
        )
    }
    .and_then(|()| texture.ok_or_else(windows::core::Error::empty))
    .map_err(failed("create a surface for NvFBC"))?;
    // SAFETY: the texture has one level.
    let surface =
        unsafe { texture.GetSurfaceLevel(0) }.map_err(failed("open the NvFBC surface"))?;
    let mut shared: Option<ID3D11Texture2D> = None;
    // SAFETY: a shared handle from a Direct3D 9Ex device on the same adapter; the
    // out-parameter is written on success.
    let shared = unsafe { device.d3d.OpenSharedResource(handle, &raw mut shared) }
        .and_then(|()| shared.ok_or_else(windows::core::Error::empty))
        .map_err(failed("share the NvFBC surface with the encoder"))?;
    // SAFETY: creates a query on a live device.
    let done = unsafe { d3d9.CreateQuery(D3DQUERYTYPE_EVENT) }
        .map_err(failed("create the NvFBC completion query"))?;
    Ok(Slot {
        surface,
        _texture: texture,
        shared,
        done,
    })
}

/// Waits until the work before `query` has finished on the GPU, which it usually has: the
/// grab was started a whole frame earlier.
fn wait_for(query: &IDirect3DQuery9) -> Result<(), CaptureError> {
    let deadline = Instant::now() + GRAB_TIMEOUT;
    loop {
        // The method's wrapper reports S_FALSE ("not yet") as success, so it is called
        // directly.
        // SAFETY: a live query; an event query takes no data buffer.
        let result = unsafe {
            (Interface::vtable(query).GetData)(query.as_raw(), ptr::null_mut(), 0, D3DGETDATA_FLUSH)
        };
        if result == S_OK {
            return Ok(());
        }
        if result.is_err() {
            return Err(failed("wait for the NvFBC grab")(result.into()));
        }
        if Instant::now() >= deadline {
            return Err(CaptureError::Unsupported(
                "the NVIDIA driver stopped completing NvFBC grabs".to_owned(),
            ));
        }
        std::thread::yield_now();
    }
}

/// Whether the monitor still exists.
fn is_attached(monitor: HMONITOR) -> bool {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>())
            .expect("monitor information is a few bytes"),
        ..Default::default()
    };
    // SAFETY: GetMonitorInfoW accepts any handle and fails for one that is gone.
    unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool()
}

/// Wraps a status the driver failed with and the step that failed, for `map_err`; a refusal
/// that trying again would not change reads as unsupported.
fn nvfbc_failed(step: &'static str) -> impl Fn(Status) -> CaptureError {
    move |status| permanent(status).unwrap_or(CaptureError::Nvfbc(step, status))
}

/// The driver's refusals that trying again would not change.
fn permanent(status: Status) -> Option<CaptureError> {
    let reason = match status {
        Status::UNSUPPORTED | Status::INVALID_TARGET => {
            "the NVIDIA driver refused NvFBC for this GPU or monitor; check that the NvFBC key is the one NVIDIA issued you"
        }
        Status::INCOMPATIBLE_DRIVER | Status::INCOMPATIBLE_VERSION => {
            "this NVIDIA driver's NvFBC does not accept Capture SDK 7 requests; update the driver"
        }
        Status::UNSUPPORTED_PLATFORM => "the NVIDIA driver does not offer NvFBC on this system",
        Status::INSUFFICIENT_PRIVILEGES => {
            "the NVIDIA driver refused NvFBC to this user; check that the NvFBC key is the one NVIDIA issued you"
        }
        Status::DYNAMIC_DISABLE => "NvFBC was turned off in the NVIDIA driver",
        _ => return None,
    };
    Some(CaptureError::Unsupported(format!("{reason} ({status})")))
}
