//! NVIDIA Frame Buffer Capture of a monitor into Direct3D 9 surfaces that NVENC reads in place
//! on the same device; the `HINDSIGHT_NVFBC_*` environment variables are experimental switches.

mod cuda;
mod ffi;

use super::{
    CaptureError, EncoderDevice, Frame, ReopenThrottle, Source, Surface, failed, is_window_closed,
    monitor_of,
};
use crate::{gpu::device::Device, log, replay::Matrix, service::Target};
use std::ptr::{self, NonNull};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE, HWND, LUID},
        Graphics::{
            Direct3D9::{
                D3D_SDK_VERSION, D3DCREATE_FPU_PRESERVE, D3DCREATE_MULTITHREADED,
                D3DCREATE_NOWINDOWCHANGES, D3DCREATE_SOFTWARE_VERTEXPROCESSING, D3DDEVTYPE_HAL,
                D3DDISPLAYMODEEX, D3DDISPLAYROTATION, D3DDISPLAYROTATION_IDENTITY, D3DFMT_A8R8G8B8,
                D3DFMT_UNKNOWN, D3DFORMAT, D3DPOOL_DEFAULT, D3DPRESENT_INTERVAL_IMMEDIATE,
                D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD, D3DUSAGE_RENDERTARGET,
                Direct3DCreate9Ex, IDirect3D9Ex, IDirect3DDevice9Ex, IDirect3DSurface9,
                IDirect3DTexture9,
            },
            Direct3D11::ID3D11Texture2D,
            Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
        },
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        UI::WindowsAndMessaging::GetDesktopWindow,
    },
    core::{BOOL, Interface, s, w},
};

pub(super) use cuda::Capture as CudaCapture;
pub(in crate::gpu) use ffi::Status;
use ffi::Zeroed;

/// The most the driver accepts; it refuses four.
const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;
const PROCESS_VARIABLE: &str = "HINDSIGHT_NVFBC_PROCESS";
const GRAB_MODE_VARIABLE: &str = "HINDSIGHT_NVFBC_GRAB_MODE";
const WAIT_TIMEOUT_VARIABLE: &str = "HINDSIGHT_NVFBC_WAIT_TIMEOUT";
const FORMAT_VARIABLE: &str = "HINDSIGHT_NVFBC_FORMAT";
/// `MAKEFOURCC('N', 'V', '1', '2')`; the Direct3D 9 headers name no constant for it.
const D3DFMT_NV12: D3DFORMAT = D3DFORMAT(u32::from_le_bytes(*b"NV12"));

pub(super) struct Capture {
    /// Must outlive the session.
    _library: Library,
    create: ffi::CreateEx,
    key: Vec<u8>,
    device: IDirect3DDevice9Ex,
    slots: [Slot; SLOTS],
    session: Option<Session>,
    /// The slot whose grab is in flight, handed over on the next tick.
    pending: Option<usize>,
    next: u32,
    handed_over: bool,
    size: (u32, u32),
    monitor: HMONITOR,
    window: Option<HWND>,
    reopen: ReopenThrottle,
    cursor: bool,
    switches: Switches,
    reported: bool,
    format: PixelFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelFormat {
    Argb,
    /// Offscreen plain surfaces; the driver converts with BT.601.
    Nv12,
}

impl PixelFormat {
    fn from_environment() -> Self {
        Self::parse(std::env::var(FORMAT_VARIABLE).ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("nv12") => Self::Nv12,
            _ => Self::Argb,
        }
    }

    const fn mode(self) -> u32 {
        match self {
            Self::Argb => ffi::TODX9VID_ARGB,
            Self::Nv12 => ffi::TODX9VID_NV12,
        }
    }
}

enum Grab {
    Started,
    /// Protected video is on screen.
    Withheld,
    Resized,
    Lost,
}

/// Experimental switches from the `HINDSIGHT_NVFBC_*` environment variables.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Switches {
    process: Option<u32>,
    grab_mode: GrabMode,
    wait_timeout: Option<u32>,
}

/// `NVFBC_TODX9VID_SOURCEMODE_*`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrabMode {
    Full,
    Scale,
    Crop,
}

impl Switches {
    const DEFAULT: Self = Self {
        process: None,
        grab_mode: GrabMode::Full,
        wait_timeout: None,
    };

    fn from_environment() -> Self {
        let variable = |name| std::env::var(name).ok();
        Self::parse(
            variable(PROCESS_VARIABLE).as_deref(),
            variable(GRAB_MODE_VARIABLE).as_deref(),
            variable(WAIT_TIMEOUT_VARIABLE).as_deref(),
        )
    }

    fn parse(process: Option<&str>, grab_mode: Option<&str>, wait_timeout: Option<&str>) -> Self {
        let value = |value: Option<&str>| {
            value
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
        };
        let grab_mode = match value(grab_mode).as_deref() {
            Some("scale") => GrabMode::Scale,
            Some("crop") => GrabMode::Crop,
            _ => GrabMode::Full,
        };
        Self {
            process: value(process).and_then(|value| value.parse().ok()),
            grab_mode,
            wait_timeout: value(wait_timeout).and_then(|value| value.parse().ok()),
        }
    }

    fn report(self) {
        if self == Self::DEFAULT {
            return;
        }
        let grab_mode = match self.grab_mode {
            GrabMode::Full => "full",
            GrabMode::Scale => "scale",
            GrabMode::Crop => "crop",
        };
        log::info(format!(
            "NvFBC experimental switches: process {}, grab mode {grab_mode}, wait timeout {}",
            self.process
                .map_or("none".to_owned(), |pid| pid.to_string()),
            self.wait_timeout
                .map_or("none".to_owned(), |milliseconds| format!(
                    "{milliseconds} ms"
                )),
        ));
    }
}

impl Capture {
    pub(super) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        let switches = Switches::from_environment();
        let format = PixelFormat::from_environment();
        let (monitor, window) = monitor_of(target);
        let key = saved_key();
        switches.report();

        // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
        let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(failed("start Direct3D 9 for NvFBC"))?;
        let adapter = find_adapter(&d3d9, device, monitor)?;
        let size = display_size(&d3d9, adapter)?;
        require_even(size, "NVIDIA NvFBC")?;
        if format == PixelFormat::Nv12 {
            log::info(
                "NvFBC grabs NV12 (HINDSIGHT_NVFBC_FORMAT=nv12): the clip signals the BT.601 matrix",
            );
        }

        let library = Library::load()?;
        let create = prepare(&library, &key, adapter)?;
        let d3d9_device = create_device(&d3d9, adapter)?;
        let slots = [
            create_slot(&d3d9_device, size, format)?,
            create_slot(&d3d9_device, size, format)?,
            create_slot(&d3d9_device, size, format)?,
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
            handed_over: false,
            size,
            monitor,
            window,
            reopen: ReopenThrottle::default(),
            cursor,
            switches,
            reported: false,
            format,
        };
        capture.session = Some(capture.open_session()?);
        Ok(capture)
    }

    fn open_session(&self) -> Result<Session, CaptureError> {
        let object = self
            .create_object()
            .map_err(nvfbc_failed("create the NvFBC session"))?;
        // Released by `Drop` should setup fail.
        let session = Session {
            object,
            buffers: Box::new(output_buffers(&self.slots)),
        };
        session
            .set_up(self.cursor, self.format)
            .map_err(nvfbc_failed("set the NvFBC session up"))?;
        Ok(session)
    }

    fn create_object(&self) -> Result<NonNull<ffi::ToDx9Vid>, Status> {
        let mut params = ffi::CreateParams::zeroed();
        params.version = ffi::CREATE_PARAMS_VER;
        params.interface_type = ffi::TO_DX9_VID;
        params.device = self.device.as_raw();
        params.private_data = self.key.as_ptr().cast();
        params.private_data_size = key_size(&self.key);
        let mut source = ffi::ProcessSource::zeroed();
        if let Some(pid) = self.switches.process {
            source.version = ffi::PROCESS_SOURCE_VER;
            source.capture_mode = ffi::CAPTURE_MODE_PROCESS;
            source.target_pid = pid;
            params.private_data2 = (&raw const source).cast();
            params.private_data2_size =
                u32::try_from(size_of::<ffi::ProcessSource>()).expect("a few hundred bytes");
        }
        // SAFETY: the device, the key and the source outlive the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        NonNull::new(params.nvfbc).ok_or(Status::UNSUPPORTED)
    }

    fn reopen(&mut self) -> Result<Frame, CaptureError> {
        let Some(now) = self.reopen.attempt() else {
            return Ok(Frame::Unchanged);
        };
        if !is_attached(self.monitor) {
            return Err(CaptureError::Closed);
        }
        match self.open_session() {
            Ok(session) => {
                self.session = Some(session);
                self.reopen.succeeded();
            }
            // Permanent refusals already read as unsupported.
            Err(CaptureError::Nvfbc(..)) => self.reopen.refused(now),
            Err(error) => return Err(error),
        }
        Ok(Frame::Unchanged)
    }

    fn grab(&mut self) -> Result<Grab, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grab::Lost);
        };
        let (status, info) = session.grab_frame(self.next, self.size, self.switches);
        if status == Status::SUCCESS && !self.reported && self.switches != Switches::DEFAULT {
            self.reported = true;
            log::info(format!(
                "NvFBC first grab: {}x{} from process {}, wait mode {}",
                info.width, info.height, info.source_pid, info.wait_mode_used
            ));
        }
        match status {
            Status::SUCCESS if info.must_recreate != 0 => Ok(Grab::Lost),
            Status::SUCCESS if (info.width, info.height) != self.size => Ok(Grab::Resized),
            Status::SUCCESS => {
                // No wait: NVENC reads on the same device, which orders the blit first.
                self.pending = Some(self.next as usize);
                self.next = (self.next + 1) % SLOT_COUNT;
                Ok(Grab::Started)
            }
            Status::INVALIDATED_SESSION => Ok(Grab::Lost),
            Status::PROTECTED_CONTENT => Ok(Grab::Withheld),
            status => Err(nvfbc_failed("grab the display with NvFBC")(status)),
        }
    }
}

impl Source for Capture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn captures_window(&self) -> bool {
        false
    }

    fn take(
        &mut self,
        _device: &Device,
        _target: Option<&ID3D11Texture2D>,
        _width: u32,
        _height: u32,
    ) -> Result<Frame, CaptureError> {
        if is_window_closed(self.window) {
            return Err(CaptureError::Closed);
        }
        if !self.handed_over {
            // A Direct3D 9 surface cannot be copied into a Direct3D 11 texture.
            return Err(CaptureError::Unsupported(
                "NVIDIA NvFBC can only encode its grabs in place".to_owned(),
            ));
        }

        let collected = self.pending.take();
        match self.grab()? {
            Grab::Started | Grab::Withheld => {}
            Grab::Resized => return Ok(Frame::Resized),
            // Deliver the collected frame first; reopen on the next tick.
            Grab::Lost if collected.is_some() => self.session = None,
            Grab::Lost => {
                self.session = None;
                return self.reopen();
            }
        }
        Ok(collected.map_or(Frame::Unchanged, Frame::InSurface))
    }

    fn hand_over_surfaces(&mut self) -> bool {
        self.handed_over = true;
        true
    }

    fn surface(&self, index: usize) -> Option<Surface> {
        let slot = self.slots.get(index)?;
        Some(Surface::Direct3D9 {
            surface: slot.surface.clone(),
            nv12: self.format == PixelFormat::Nv12,
        })
    }

    fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Direct3D9(self.device.clone())
    }

    fn colour_matrix(&self) -> Matrix {
        match self.format {
            PixelFormat::Argb => Matrix::Bt709,
            PixelFormat::Nv12 => Matrix::Bt601,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // The session goes before the library that implements it.
        self.session = None;
    }
}

/// `NvFBC64.dll`; everything looked up in or made by it must be gone before it is dropped.
struct Library(HMODULE);

impl Library {
    fn load() -> Result<Self, CaptureError> {
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
        // SAFETY: loaded by `load`; `Capture` releases its session first.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

struct EntryPoints {
    get_status: ffi::GetStatusEx,
    create: ffi::CreateEx,
    enable: ffi::Enable,
}

impl EntryPoints {
    fn look_up(library: &Library) -> Result<Self, CaptureError> {
        let library = library.0;
        let missing = |name: &str| {
            CaptureError::Unsupported(format!(
                "NvFBC64.dll has no {name}; update the NVIDIA driver"
            ))
        };
        // SAFETY: each symbol has the documented signature.
        unsafe {
            let get_status = GetProcAddress(library, s!("NvFBC_GetStatusEx"))
                .ok_or_else(|| missing("NvFBC_GetStatusEx"))?;
            let create = GetProcAddress(library, s!("NvFBC_CreateEx"))
                .ok_or_else(|| missing("NvFBC_CreateEx"))?;
            let enable = GetProcAddress(library, s!("NvFBC_Enable"))
                .ok_or_else(|| missing("NvFBC_Enable"))?;
            Ok(Self {
                get_status: std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    ffi::GetStatusEx,
                >(get_status),
                create: std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::CreateEx>(
                    create,
                ),
                enable: std::mem::transmute::<unsafe extern "system" fn() -> isize, ffi::Enable>(
                    enable,
                ),
            })
        }
    }
}

struct Slot {
    surface: IDirect3DSurface9,
    /// Keeps an ARGB surface's parent texture alive; `None` for NV12.
    _texture: Option<IDirect3DTexture9>,
}

struct Session {
    /// Live until `Drop`; its method table lives in the library.
    object: NonNull<ffi::ToDx9Vid>,
    /// The driver may read these for the session's life.
    buffers: Box<[ffi::OutputBuffer; SLOTS]>,
}

impl Session {
    fn methods(&self) -> &ffi::ToDx9VidVtable {
        // SAFETY: the object is live and the library holding its table is loaded.
        unsafe { &*(*self.object.as_ptr()).vtable }
    }

    fn set_up(&self, cursor: bool, format: PixelFormat) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::TODX9VID_SETUP_PARAMS_VER;
        setup.flags = if cursor { ffi::SETUP_WITH_HW_CURSOR } else { 0 };
        setup.mode = format.mode();
        setup.buffer_count = SLOT_COUNT;
        setup.buffers = self.buffers.as_ptr().cast_mut();
        // SAFETY: a live object; the surfaces are on its device and outlive the session.
        unsafe { (self.methods().set_up)(self.object.as_ptr(), &raw mut setup) }.ok()
    }

    fn grab_frame(
        &self,
        buffer_index: u32,
        size: (u32, u32),
        switches: Switches,
    ) -> (Status, ffi::FrameGrabInfo) {
        let mut info = ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::TODX9VID_GRAB_FRAME_PARAMS_VER;
        match switches.wait_timeout {
            Some(milliseconds) => {
                params.flags = ffi::TODX9VID_WAIT_WITH_TIMEOUT;
                params.wait_time = milliseconds;
            }
            None => params.flags = ffi::TODX9VID_NOWAIT,
        }
        // At the display's own size, scale and crop deliver the full picture.
        params.grab_mode = match switches.grab_mode {
            GrabMode::Full => ffi::TODX9VID_SOURCEMODE_FULL,
            GrabMode::Scale => ffi::TODX9VID_SOURCEMODE_SCALE,
            GrabMode::Crop => ffi::TODX9VID_SOURCEMODE_CROP,
        };
        if switches.grab_mode != GrabMode::Full {
            (params.target_width, params.target_height) = size;
        }
        params.buffer_index = buffer_index;
        params.grab_info = &raw mut info;
        // SAFETY: a live, set-up object; `info` outlives the call.
        let status = unsafe { (self.methods().grab_frame)(self.object.as_ptr(), &raw mut params) };
        (status, info)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live object from `NvFBC_CreateEx`; nothing uses it afterwards.
        let _ = unsafe { (self.methods().release)(self.object.as_ptr()) };
    }
}

// The driver's key, masked in the binary and unmasked at use.
const MASKED_KEY: [u8; 16] = [
    0x58, 0x01, 0xDB, 0x6A, 0x68, 0x12, 0xEE, 0x2A, 0x09, 0x77, 0x6C, 0x2B, 0x3E, 0x91, 0x06, 0xEC,
];
const KEY_MASK: [u8; 16] = [
    0x9D, 0x7B, 0x2E, 0xC4, 0x51, 0x08, 0xF3, 0x6A, 0xB7, 0x1C, 0xE9, 0x30, 0x84, 0x5F, 0xD6, 0x72,
];

fn saved_key() -> Vec<u8> {
    MASKED_KEY
        .iter()
        .zip(KEY_MASK)
        .map(|(byte, mask)| byte ^ mask)
        .collect()
}

fn key_size(key: &[u8]) -> u32 {
    u32::try_from(key.len()).unwrap_or(u32::MAX)
}

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

fn prepare(library: &Library, key: &[u8], adapter: u32) -> Result<ffi::CreateEx, CaptureError> {
    let entry_points = EntryPoints::look_up(library)?;
    if !is_capture_possible(entry_points.get_status, key, adapter)? {
        turn_on(entry_points.enable)?;
    }
    Ok(entry_points.create)
}

fn is_capture_possible(
    get_status: ffi::GetStatusEx,
    key: &[u8],
    adapter: u32,
) -> Result<bool, CaptureError> {
    let mut status = ffi::StatusEx::zeroed();
    status.version = ffi::STATUS_VER;
    status.adapter_index = adapter;
    status.private_data = key.as_ptr().cast();
    status.private_data_size = key_size(key);
    // SAFETY: the key outlives the call.
    unsafe { get_status(&raw mut status) }
        .ok()
        .map_err(nvfbc_failed("read the NvFBC status"))?;
    Ok(status.flags & ffi::STATUS_CAPTURE_POSSIBLE != 0)
}

fn turn_on(enable: ffi::Enable) -> Result<(), CaptureError> {
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
    Ok(())
}

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
    // Software vertex processing: with hardware, the NVIDIA driver runs a worker thread for
    // the device, and nothing here draws.
    let flags = D3DCREATE_FPU_PRESERVE
        | D3DCREATE_MULTITHREADED
        | D3DCREATE_SOFTWARE_VERTEXPROCESSING
        | D3DCREATE_NOWINDOWCHANGES;
    let mut device = None;
    // SAFETY: windowed parameters on the desktop window; out-parameter written on success.
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

fn create_slot(
    d3d9: &IDirect3DDevice9Ex,
    size: (u32, u32),
    format: PixelFormat,
) -> Result<Slot, CaptureError> {
    match format {
        PixelFormat::Argb => {
            let texture = create_texture(d3d9, size)?;
            // SAFETY: the texture has one level.
            let surface =
                unsafe { texture.GetSurfaceLevel(0) }.map_err(failed("open the NvFBC surface"))?;
            Ok(Slot {
                surface,
                _texture: Some(texture),
            })
        }
        PixelFormat::Nv12 => Ok(Slot {
            surface: create_nv12_surface(d3d9, size)?,
            _texture: None,
        }),
    }
}

/// Direct3D 9 makes NV12 neither as a texture nor as a render target.
fn create_nv12_surface(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<IDirect3DSurface9, CaptureError> {
    let mut surface = None;
    // SAFETY: unshared, so the handle pointer may be null; out-parameter written on success.
    unsafe {
        d3d9.CreateOffscreenPlainSurface(
            width,
            height,
            D3DFMT_NV12,
            D3DPOOL_DEFAULT,
            &raw mut surface,
            ptr::null_mut(),
        )
    }
    .and_then(|()| surface.ok_or_else(windows::core::Error::empty))
    .map_err(failed("create an NV12 surface for NvFBC"))
}

fn create_texture(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<IDirect3DTexture9, CaptureError> {
    let mut texture = None;
    // SAFETY: unshared, so the handle pointer may be null; out-parameter written on success.
    unsafe {
        d3d9.CreateTexture(
            width,
            height,
            1,
            D3DUSAGE_RENDERTARGET.cast_unsigned(),
            D3DFMT_A8R8G8B8,
            D3DPOOL_DEFAULT,
            &raw mut texture,
            ptr::null_mut(),
        )
    }
    .and_then(|()| texture.ok_or_else(windows::core::Error::empty))
    .map_err(failed("create a surface for NvFBC"))
}

fn output_buffers(slots: &[Slot; SLOTS]) -> [ffi::OutputBuffer; SLOTS] {
    std::array::from_fn(|index| {
        let mut buffer = ffi::OutputBuffer::zeroed();
        buffer.primary = slots[index].surface.as_raw();
        buffer
    })
}

fn is_attached(monitor: HMONITOR) -> bool {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>())
            .expect("monitor information is a few bytes"),
        ..Default::default()
    };
    // SAFETY: GetMonitorInfoW accepts any handle and fails for one that is gone.
    unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool()
}

/// NVENC encodes even sizes, and grabs are read in place, uncropped.
fn require_even((width, height): (u32, u32), backend: &str) -> Result<(), CaptureError> {
    if width % 2 == 0 && height % 2 == 0 {
        return Ok(());
    }
    Err(CaptureError::Unsupported(format!(
        "the display is {width}×{height}; {backend} encodes its grabs in place and needs an even width and height, so choose Windows Graphics Capture"
    )))
}

fn nvfbc_failed(step: &'static str) -> impl Fn(Status) -> CaptureError {
    move |status| permanent(status).unwrap_or(CaptureError::Nvfbc(step, status))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_default_when_unset_or_malformed() {
        assert!(Switches::parse(None, None, None) == Switches::DEFAULT);
        assert!(Switches::parse(Some(""), Some("  "), Some("")) == Switches::DEFAULT);
        assert!(Switches::parse(Some("game"), Some("tiled"), Some("-1")) == Switches::DEFAULT);
    }

    #[test]
    fn switches_read_their_values_loosely() {
        let switches = Switches::parse(Some(" 4321 "), Some("Crop"), Some("16"));
        assert_eq!(switches.process, Some(4321));
        assert!(switches.grab_mode == GrabMode::Crop);
        assert_eq!(switches.wait_timeout, Some(16));
        assert!(Switches::parse(None, Some("SCALE"), None).grab_mode == GrabMode::Scale);
    }

    #[test]
    fn only_nv12_chooses_the_nv12_format() {
        assert_eq!(PixelFormat::parse(None), PixelFormat::Argb);
        assert_eq!(PixelFormat::parse(Some("")), PixelFormat::Argb);
        assert_eq!(PixelFormat::parse(Some("argb")), PixelFormat::Argb);
        assert_eq!(PixelFormat::parse(Some("nv21")), PixelFormat::Argb);
        assert_eq!(PixelFormat::parse(Some(" NV12 ")), PixelFormat::Nv12);
        assert_eq!(PixelFormat::Nv12.mode(), ffi::TODX9VID_NV12);
        assert_eq!(PixelFormat::Argb.mode(), ffi::TODX9VID_ARGB);
    }

    #[test]
    fn nv12_names_its_fourcc() {
        assert_eq!(D3DFMT_NV12.0, 0x3231_564E);
    }

    #[test]
    fn odd_displays_are_refused_with_a_way_out() {
        assert!(require_even((2560, 1440), "NVIDIA NvFBC").is_ok());
        for size in [(1367, 768), (1366, 767)] {
            let Err(CaptureError::Unsupported(message)) = require_even(size, "NVIDIA NvFBC") else {
                panic!("an odd size is refused as unsupported");
            };
            assert!(message.contains("Windows Graphics Capture"));
        }
    }

    #[test]
    fn permanent_refusals_read_as_unsupported_and_others_keep_their_step() {
        assert!(matches!(
            nvfbc_failed("step")(Status::UNSUPPORTED),
            CaptureError::Unsupported(_)
        ));
        assert!(matches!(
            nvfbc_failed("step")(Status::INVALIDATED_SESSION),
            CaptureError::Nvfbc("step", Status::INVALIDATED_SESSION)
        ));
        assert!(permanent(Status::PROTECTED_CONTENT).is_none());
    }

    #[test]
    fn the_key_is_unmasked_at_use_and_never_stored_in_the_clear() {
        let key = saved_key();
        assert_eq!(key.len(), MASKED_KEY.len());
        assert_ne!(key.as_slice(), MASKED_KEY.as_slice());
        let remasked = key
            .iter()
            .zip(KEY_MASK)
            .map(|(byte, mask)| byte ^ mask)
            .collect::<Vec<_>>();
        assert_eq!(remasked, MASKED_KEY);
    }
}
