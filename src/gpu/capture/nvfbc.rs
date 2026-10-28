//! NVIDIA Frame Buffer Capture of a monitor into Direct3D 9 surfaces that NVENC reads in place
//! on the same device.

mod cuda;
mod ffi;

use super::{
    CaptureError, CopyTarget, D3DFMT_NV12, EncoderDevice, Format, Frame, ReopenThrottle, Source,
    Surface, failed, monitor_of,
};
use crate::{
    gpu::device::Device,
    log,
    replay::Matrix,
    service::{Target, WindowHandle},
    sys::win32::cb_size,
};
use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    sync::Arc,
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::{HWND, LUID},
        Graphics::{
            Direct3D9::{
                D3D_SDK_VERSION, D3DCREATE_FPU_PRESERVE, D3DCREATE_MULTITHREADED,
                D3DCREATE_NOWINDOWCHANGES, D3DCREATE_SOFTWARE_VERTEXPROCESSING, D3DDEVTYPE_HAL,
                D3DDISPLAYMODEEX, D3DDISPLAYROTATION, D3DDISPLAYROTATION_IDENTITY, D3DFMT_A8R8G8B8,
                D3DFMT_UNKNOWN, D3DPOOL_DEFAULT, D3DPRESENT_INTERVAL_IMMEDIATE,
                D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD, D3DUSAGE_RENDERTARGET,
                Direct3DCreate9Ex, IDirect3D9Ex, IDirect3DDevice9Ex, IDirect3DSurface9,
                IDirect3DTexture9,
            },
            Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
        },
        Media::MediaFoundation::{
            DXVA2_VideoDecoderRenderTarget, DXVA2CreateVideoService,
            IDirectXVideoAccelerationService,
        },
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    },
    core::{BOOL, Interface, s, w},
};

use crate::gpu::ffi::{Library, Symbol};
use bytemuck::Zeroable;
pub(super) use cuda::Capture as CudaCapture;
pub(in crate::gpu) use ffi::Status;

/// The most buffers the driver accepts. It refuses four.
const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;

pub(super) struct Capture {
    /// Declared first: released before the surfaces it grabs into and their device.
    session: Option<Session>,
    /// The full-screen app whose own frames are grabbed, or `None` to grab the monitor. Boxed so
    /// that its address stays fixed for the driver.
    process: Option<Box<ffi::ProcessTarget>>,
    create: ffi::CreateEx,
    key: Key,
    device: IDirect3DDevice9Ex,
    slots: [Slot; SLOTS],
    format: Format,
    /// The slot whose grab is in flight, delivered on the next tick.
    pending: Option<usize>,
    next: u32,
    handed_over: bool,
    size: (u32, u32),
    monitor: HMONITOR,
    reopen: ReopenThrottle,
    cursor: bool,
    /// Declared last: `create` points into it. Each session holds it too.
    library: Arc<Library>,
}

impl Capture {
    pub(super) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        let Prepared {
            library,
            create,
            key,
            adapter,
            d3d9,
            size,
            monitor,
            set_global_flags,
        } = prepare_capture(device, target, "NVIDIA NvFBC")?;
        let process = process_target(target, set_global_flags);
        let d3d9_device = create_device(&d3d9, adapter)?;
        let (format, slots) = create_slots(&d3d9_device, size)?;

        let mut capture = Self {
            session: None,
            process,
            create,
            key,
            device: d3d9_device,
            slots,
            format,
            pending: None,
            next: 0,
            handed_over: false,
            size,
            monitor,
            reopen: ReopenThrottle::default(),
            cursor,
            library,
        };
        capture.session = Some(capture.open_first_session()?);
        Ok(capture)
    }

    /// Gives up grabbing the app's own frames, then grabbing NV12, if the session cannot open
    /// with them.
    fn open_first_session(&mut self) -> Result<Session, CaptureError> {
        let mut opened = self.open_session();
        if let Err(error) = &opened
            && self.process.is_some()
        {
            log::info(format!(
                "NvFBC · cannot grab the app's own frames ({error}) · grabbing the monitor"
            ));
            self.process = None;
            opened = self.open_session();
        }
        if let Err(error) = &opened
            && self.format == Format::Nv12
        {
            log::info(format!(
                "NvFBC · cannot grab NV12 ({error}) · grabbing BGRA"
            ));
            self.slots = texture_slots(&self.device, self.size)?;
            self.format = Format::Bgra;
            opened = self.open_session();
        }
        opened
    }

    fn open_session(&self) -> Result<Session, CaptureError> {
        let object = self
            .create_object()
            .map_err(nvfbc_failed("create the NvFBC session"))?;
        // Released by `Drop` if setup fails.
        let session = Session {
            object,
            buffers: OutputTable::new(output_buffers(&self.slots)),
        };
        session
            .set_up(self.cursor, self.format)
            .map_err(nvfbc_failed("set the NvFBC session up"))?;
        Ok(session)
    }

    fn create_object(&self) -> Result<Object<ffi::ToDx9VidVtable>, Status> {
        let mut params = ffi::CreateParams::zeroed();
        params.version = ffi::CREATE_PARAMS_VER;
        params.interface_type = ffi::TO_DX9_VID;
        params.device = self.device.as_raw();
        params.private_data = self.key.as_ptr().cast();
        params.private_data_size = KEY_BYTES;
        if let Some(process) = &self.process {
            params.private_data2 = (&raw const **process).cast();
            params.private_data2_size = cb_size::<ffi::ProcessTarget>();
        }
        // SAFETY: the device, the key and the process target outlive the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        Object::new(params.nvfbc, &self.library).ok_or(Status::UNSUPPORTED)
    }

    fn reopen(&mut self) -> Result<Frame, CaptureError> {
        if let Some(now) = reopen_due(&self.reopen, self.monitor)? {
            let opened = self.open_session();
            if let Some(session) = note_reopened(&mut self.reopen, now, opened)? {
                self.session = Some(session);
            }
        }
        Ok(Frame::Unchanged)
    }

    fn grab(&mut self) -> Result<Grabbed, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grabbed::Lost);
        };
        let (status, info) = session.grab_frame(self.next);
        let grabbed = classify_grab(status, &info, self.size, "grab the display with NvFBC")?;
        if grabbed == Grabbed::Frame {
            // No wait is needed: NVENC reads on the same device, which orders the blit first.
            self.pending = Some(self.next as usize);
            self.next = (self.next + 1) % SLOT_COUNT;
        }
        Ok(grabbed)
    }
}

impl Source for Capture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn captures_window(&self) -> bool {
        false
    }

    fn take(&mut self, _target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError> {
        if !self.handed_over {
            // A Direct3D 9 surface cannot be copied into a Direct3D 11 texture.
            return Err(CaptureError::Unsupported(
                "NVIDIA NvFBC can only encode its grabs in place".to_owned(),
            ));
        }

        let collected = self.pending.take();
        match self.grab()? {
            Grabbed::Frame | Grabbed::Withheld => {}
            // The app presents at a size other than the monitor's, which the monitor's surfaces
            // cannot hold.
            Grabbed::Resized if self.process.is_some() => {
                log::info(
                    "NvFBC · the app's frames are not the monitor's size · grabbing the monitor",
                );
                self.process = None;
                self.session = None;
                return self.reopen();
            }
            Grabbed::Resized => return Ok(Frame::Resized),
            // Deliver the collected frame first; reopen on the next tick.
            Grabbed::Lost if collected.is_some() => self.session = None,
            Grabbed::Lost => {
                self.session = None;
                return self.reopen();
            }
        }
        Ok(
            collected.map_or(Frame::Unchanged, |index| Frame::InSurface {
                index,
                surface: Surface::Direct3D9 {
                    surface: self.slots[index].surface.clone(),
                    format: self.format,
                },
            }),
        )
    }

    fn hold(&mut self) {
        self.pending = None;
    }
}

impl Capture {
    pub(super) fn hand_over_surfaces(&mut self) -> bool {
        self.handed_over = true;
        true
    }

    pub(super) fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    pub(super) fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Direct3D9(self.device.clone())
    }

    pub(super) const fn colour_matrix(&self) -> Matrix {
        match self.format {
            Format::Bgra => Matrix::Bt709,
            Format::Nv12 => Matrix::Bt601,
        }
    }
}

/// The state both backends need before their first session.
struct Prepared {
    library: Arc<Library>,
    create: ffi::CreateEx,
    key: Key,
    /// The Direct3D 9 adapter ordinal, which the driver counts by.
    adapter: u32,
    d3d9: IDirect3D9Ex,
    size: (u32, u32),
    monitor: HMONITOR,
    /// `None` when the library does not export it.
    set_global_flags: Option<ffi::SetGlobalFlags>,
}

/// Finds the monitor on the encoding GPU, loads the library and turns NvFBC on if necessary.
fn prepare_capture(
    device: &Device,
    target: &Target,
    backend: &str,
) -> Result<Prepared, CaptureError> {
    let monitor = monitor_of(target);
    let key = saved_key();

    // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
    let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
        .map_err(failed("start Direct3D 9 for NvFBC"))?;
    let adapter = find_adapter(&d3d9, device, monitor)?;
    let size = display_size(&d3d9, adapter)?;
    require_even(size, backend)?;

    let library = load_library()?;
    let entry_points = EntryPoints::look_up(&library)?;
    if !is_capture_possible(*entry_points.get_status, &key, adapter)? {
        turn_on(*entry_points.enable)?;
    }
    // Detached from the borrow: each capture keeps the library in its last field.
    let create = *entry_points.create;
    let set_global_flags = entry_points.set_global_flags.as_deref().copied();
    Ok(Prepared {
        library: Arc::new(library),
        create,
        key,
        adapter,
        d3d9,
        size,
        monitor,
        set_global_flags,
    })
}

/// Targets the full-screen app's process, so that NvFBC grabs the frames the app presents rather
/// than the monitor. The NVIDIA driver already loaded in the app makes the copy; nothing is
/// injected into it.
fn process_target(
    target: &Target,
    set_global_flags: Option<ffi::SetGlobalFlags>,
) -> Option<Box<ffi::ProcessTarget>> {
    let Target::Window { window, name, .. } = target else {
        return None;
    };
    let process_id = process_of(*window)?;
    if let Some(set_global_flags) = set_global_flags {
        // SAFETY: `NvFBC_SetGlobalFlags` from the library the caller holds; the flags go by value.
        let _ = unsafe { set_global_flags(ffi::GLOBAL_FLAGS_NO_INITIAL_REFRESH) };
    }
    log::info(format!("NvFBC · grabbing the frames {name} presents"));
    let mut process = Box::new(ffi::ProcessTarget::zeroed());
    process.version = ffi::PROCESS_TARGET_VER;
    process.capture_mode = ffi::CAPTURE_MODE_PROCESS;
    process.process_id = process_id;
    Some(process)
}

fn process_of(window: WindowHandle) -> Option<u32> {
    let mut process_id = 0;
    // SAFETY: GetWindowThreadProcessId accepts any handle and writes the id only for a live one.
    unsafe {
        GetWindowThreadProcessId(HWND(window.get() as *mut c_void), Some(&raw mut process_id))
    };
    (process_id != 0).then_some(process_id)
}

fn load_library() -> Result<Library, CaptureError> {
    Library::load(w!("NvFBC64.dll")).map_err(|_| {
        CaptureError::Unsupported(
            "NvFBC64.dll was not found; install or update the NVIDIA driver".to_owned(),
        )
    })
}

struct EntryPoints<'lib> {
    get_status: Symbol<'lib, ffi::GetStatusEx>,
    create: Symbol<'lib, ffi::CreateEx>,
    enable: Symbol<'lib, ffi::Enable>,
    set_global_flags: Option<Symbol<'lib, ffi::SetGlobalFlags>>,
}

impl<'lib> EntryPoints<'lib> {
    fn look_up(library: &'lib Library) -> Result<Self, CaptureError> {
        let missing = |name: &str| {
            CaptureError::Unsupported(format!(
                "NvFBC64.dll has no {name}; update the NVIDIA driver"
            ))
        };
        // SAFETY: declared `NVFBCRESULT NVFBCAPI (NvFBCStatusEx*)` in NvFBC.h.
        let get_status = unsafe { library.export(s!("NvFBC_GetStatusEx")) }
            .ok_or_else(|| missing("NvFBC_GetStatusEx"))?;
        // SAFETY: declared `NVFBCRESULT NVFBCAPI (void*)` in NvFBC.h, taking `NvFBCCreateParams`.
        let create = unsafe { library.export(s!("NvFBC_CreateEx")) }
            .ok_or_else(|| missing("NvFBC_CreateEx"))?;
        // SAFETY: declared `NVFBCRESULT NVFBCAPI (NVFBC_STATE)` in NvFBC.h.
        let enable =
            unsafe { library.export(s!("NvFBC_Enable")) }.ok_or_else(|| missing("NvFBC_Enable"))?;
        // SAFETY: `NVFBCRESULT NVFBCAPI (DWORD)`, as NVIDIA's capture engine calls it.
        let set_global_flags = unsafe { library.export(s!("NvFBC_SetGlobalFlags")) };
        Ok(Self {
            get_status,
            create,
            enable,
            set_global_flags,
        })
    }
}

/// A live interface object from `NvFBC_CreateEx`, released on drop.
struct Object<V: ffi::Vtable> {
    pointer: NonNull<ffi::Interface<V>>,
    /// The library that implements the object and holds its method table; dropped after the
    /// release in `Drop`.
    _library: Arc<Library>,
}

impl<V: ffi::Vtable> Object<V> {
    fn new(pointer: *mut c_void, library: &Arc<Library>) -> Option<Self> {
        Some(Self {
            pointer: NonNull::new(pointer.cast())?,
            _library: Arc::clone(library),
        })
    }

    fn as_ptr(&self) -> *mut ffi::Interface<V> {
        self.pointer.as_ptr()
    }

    fn methods(&self) -> &V {
        // SAFETY: the object is live until `Drop` releases it, so its header can be read.
        let vtable = unsafe { (*self.pointer.as_ptr()).vtable };
        // SAFETY: the table lives in the library, which `self` keeps loaded, and is never
        // written.
        unsafe { &*vtable }
    }
}

impl<V: ffi::Vtable> Drop for Object<V> {
    fn drop(&mut self) {
        // SAFETY: a live object from `NvFBC_CreateEx`; nothing uses it afterwards.
        let _ = unsafe { (self.methods().release())(self.pointer.as_ptr()) };
    }
}

struct Slot {
    surface: IDirect3DSurface9,
    /// Keeps a BGRA surface's parent texture alive.
    _texture: Option<IDirect3DTexture9>,
}

struct Session {
    /// Declared first: released before the table it was set up with is freed.
    object: Object<ffi::ToDx9VidVtable>,
    buffers: OutputTable,
}

/// The table of output buffers a session is set up with, which the driver keeps a pointer to
/// for the session's life: allocated once, never moved, freed on drop.
struct OutputTable(NonNull<[ffi::OutputBuffer; SLOTS]>);

impl OutputTable {
    fn new(buffers: [ffi::OutputBuffer; SLOTS]) -> Self {
        Self(NonNull::from(Box::leak(Box::new(buffers))))
    }

    /// Returns a pointer for the driver, which may read and write through it.
    const fn as_mut_ptr(&self) -> *mut ffi::OutputBuffer {
        self.0.as_ptr().cast()
    }
}

impl Drop for OutputTable {
    fn drop(&mut self) {
        // SAFETY: leaked from a `Box` in `new` and freed only here; the session that handed the
        // pointer to the driver was released first.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl Session {
    fn set_up(&self, cursor: bool, format: Format) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::TODX9VID_SETUP_PARAMS_VER;
        setup.flags = if cursor { ffi::SETUP_WITH_HW_CURSOR } else { 0 };
        setup.mode = match format {
            Format::Bgra => ffi::TODX9VID_ARGB,
            Format::Nv12 => ffi::TODX9VID_NV12,
        };
        setup.buffer_count = SLOT_COUNT;
        setup.buffers = self.buffers.as_mut_ptr();
        // SAFETY: a live object; the surfaces are on its device and outlive the session.
        unsafe { (self.object.methods().set_up)(self.object.as_ptr(), &raw mut setup) }.ok()
    }

    fn grab_frame(&self, buffer_index: u32) -> (Status, ffi::FrameGrabInfo) {
        let mut info = ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::TODX9VID_GRAB_FRAME_PARAMS_VER;
        params.flags = ffi::TODX9VID_NOWAIT;
        params.buffer_index = buffer_index;
        params.grab_info = &raw mut info;
        // SAFETY: a live, set-up object; `info` outlives the call.
        let status =
            unsafe { (self.object.methods().grab_frame)(self.object.as_ptr(), &raw mut params) };
        (status, info)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grabbed {
    /// The picture is in the requested buffer.
    Frame,
    /// Protected video is on screen.
    Withheld,
    Resized,
    Lost,
}

fn classify_grab(
    status: Status,
    info: &ffi::FrameGrabInfo,
    size: (u32, u32),
    step: &'static str,
) -> Result<Grabbed, CaptureError> {
    match status {
        Status::SUCCESS if info.must_recreate != 0 => Ok(Grabbed::Lost),
        Status::SUCCESS if (info.width, info.height) != size => Ok(Grabbed::Resized),
        Status::SUCCESS => Ok(Grabbed::Frame),
        Status::INVALIDATED_SESSION => Ok(Grabbed::Lost),
        Status::PROTECTED_CONTENT => Ok(Grabbed::Withheld),
        status => Err(nvfbc_failed(step)(status)),
    }
}

/// Returns `Some(now)` when a lost session may be opened again. A monitor that is gone ends the
/// capture.
fn reopen_due(
    throttle: &ReopenThrottle,
    monitor: HMONITOR,
) -> Result<Option<Instant>, CaptureError> {
    let Some(now) = throttle.attempt() else {
        return Ok(None);
    };
    if !is_attached(monitor) {
        return Err(CaptureError::Closed);
    }
    Ok(Some(now))
}

/// A passing refusal delays the next attempt. Permanent refusals are already classified as
/// unsupported.
fn note_reopened<S>(
    throttle: &mut ReopenThrottle,
    now: Instant,
    opened: Result<S, CaptureError>,
) -> Result<Option<S>, CaptureError> {
    match opened {
        Ok(session) => {
            throttle.succeeded();
            Ok(Some(session))
        }
        Err(CaptureError::Nvfbc(..)) => {
            throttle.refused(now);
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

const KEY_BYTES: u32 = 16;
type Key = [u8; KEY_BYTES as usize];

// The driver's key, masked in the source only: the optimiser folds the unmasking, so a release
// binary holds the key in the clear.
const MASKED_KEY: Key = [
    0x58, 0x01, 0xDB, 0x6A, 0x68, 0x12, 0xEE, 0x2A, 0x09, 0x77, 0x6C, 0x2B, 0x3E, 0x91, 0x06, 0xEC,
];
const KEY_MASK: Key = [
    0x9D, 0x7B, 0x2E, 0xC4, 0x51, 0x08, 0xF3, 0x6A, 0xB7, 0x1C, 0xE9, 0x30, 0x84, 0x5F, 0xD6, 0x72,
];

fn saved_key() -> Key {
    std::array::from_fn(|index| MASKED_KEY[index] ^ KEY_MASK[index])
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
        Size: cb_size::<D3DDISPLAYMODEEX>(),
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

fn is_capture_possible(
    get_status: ffi::GetStatusEx,
    key: &Key,
    adapter: u32,
) -> Result<bool, CaptureError> {
    let mut status = ffi::StatusEx::zeroed();
    status.version = ffi::STATUS_VER;
    status.adapter_index = adapter;
    status.private_data = key.as_ptr().cast();
    status.private_data_size = KEY_BYTES;
    // SAFETY: the key outlives the call.
    unsafe { get_status(&raw mut status) }
        .ok()
        .map_err(nvfbc_failed("read the NvFBC status"))?;
    Ok(status.flags & ffi::STATUS_CAPTURE_POSSIBLE != 0)
}

fn turn_on(enable: ffi::Enable) -> Result<(), CaptureError> {
    // SAFETY: `enable` is `NvFBC_Enable` from the library `prepare_capture` still holds, and
    // takes the state by value.
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
    log::info("NvFBC · turned on in the NVIDIA driver");
    Ok(())
}

fn create_device(d3d9: &IDirect3D9Ex, adapter: u32) -> Result<IDirect3DDevice9Ex, CaptureError> {
    let mut parameters = D3DPRESENT_PARAMETERS {
        BackBufferWidth: 1,
        BackBufferHeight: 1,
        BackBufferFormat: D3DFMT_UNKNOWN,
        BackBufferCount: 1,
        SwapEffect: D3DSWAPEFFECT_DISCARD,
        hDeviceWindow: HWND(winsafe::HWND::GetDesktopWindow().ptr()),
        Windowed: BOOL::from(true),
        PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        ..Default::default()
    };
    // Software vertex processing is used because with hardware vertex processing the NVIDIA
    // driver runs a worker thread for the device, and nothing here draws.
    let flags = D3DCREATE_FPU_PRESERVE
        | D3DCREATE_MULTITHREADED
        | D3DCREATE_SOFTWARE_VERTEXPROCESSING
        | D3DCREATE_NOWINDOWCHANGES;
    let mut device = None;
    // SAFETY: windowed parameters on the desktop window. The out-parameter is written on success.
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

/// NV12 surfaces of the kind video decoders write, which NVENC reads as they are. From BGRA, or
/// from NV12 in plain surfaces, NVENC first runs a pass of its own on the 3D engine, which costs
/// a GPU-bound game frames.
fn create_slots(
    d3d9: &IDirect3DDevice9Ex,
    size: (u32, u32),
) -> Result<(Format, [Slot; SLOTS]), CaptureError> {
    match video_slots(d3d9, size) {
        Ok(slots) => Ok((Format::Nv12, slots)),
        Err(error) => {
            log::info(format!(
                "NvFBC · no NV12 surfaces ({error}) · grabbing BGRA"
            ));
            Ok((Format::Bgra, texture_slots(d3d9, size)?))
        }
    }
}

fn video_slots(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<[Slot; SLOTS], CaptureError> {
    let step = "create NV12 surfaces for NvFBC";
    let mut service = ptr::null_mut();
    // SAFETY: a live device; the out-parameter is written on success.
    unsafe {
        DXVA2CreateVideoService(
            d3d9,
            &IDirectXVideoAccelerationService::IID,
            &raw mut service,
        )
    }
    .map_err(failed(step))?;
    // SAFETY: the call succeeded, so `service` is an owned IDirectXVideoAccelerationService.
    let service = unsafe { IDirectXVideoAccelerationService::from_raw(service) };
    let mut surfaces: [Option<IDirect3DSurface9>; SLOTS] = Default::default();
    // SAFETY: the array has room for the back buffers and the front one; no shared handle.
    unsafe {
        service.CreateSurface(
            width,
            height,
            SLOT_COUNT - 1,
            D3DFMT_NV12,
            D3DPOOL_DEFAULT,
            0,
            DXVA2_VideoDecoderRenderTarget,
            surfaces.as_mut_ptr(),
            None,
        )
    }
    .map_err(failed(step))?;
    let [Some(first), Some(second), Some(third)] = surfaces else {
        return Err(failed(step)(windows::core::Error::empty()));
    };
    Ok([first, second, third].map(|surface| Slot {
        surface,
        _texture: None,
    }))
}

fn texture_slots(
    d3d9: &IDirect3DDevice9Ex,
    size: (u32, u32),
) -> Result<[Slot; SLOTS], CaptureError> {
    Ok([
        texture_slot(d3d9, size)?,
        texture_slot(d3d9, size)?,
        texture_slot(d3d9, size)?,
    ])
}

fn texture_slot(d3d9: &IDirect3DDevice9Ex, size: (u32, u32)) -> Result<Slot, CaptureError> {
    let texture = create_texture(d3d9, size)?;
    // SAFETY: the texture has one level.
    let surface =
        unsafe { texture.GetSurfaceLevel(0) }.map_err(failed("open the NvFBC surface"))?;
    Ok(Slot {
        surface,
        _texture: Some(texture),
    })
}

fn create_texture(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<IDirect3DTexture9, CaptureError> {
    let mut texture = None;
    // SAFETY: the texture is unshared, so the handle pointer may be null. The out-parameter is
    // written on success.
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
        cbSize: cb_size::<MONITORINFO>(),
        ..Default::default()
    };
    // SAFETY: GetMonitorInfoW accepts any handle and fails for one that is gone.
    unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool()
}

/// Requires an even size: NVENC encodes even sizes, and grabs are read in place, uncropped.
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
    fn a_grab_is_classified_by_its_status_and_the_picture_it_reports() {
        let size = (1920, 1080);
        let mut info = ffi::FrameGrabInfo::zeroed();
        (info.width, info.height) = size;
        let classify =
            |status, info: &ffi::FrameGrabInfo| classify_grab(status, info, size, "step");
        assert_eq!(classify(Status::SUCCESS, &info).unwrap(), Grabbed::Frame);
        assert_eq!(
            classify(Status::PROTECTED_CONTENT, &info).unwrap(),
            Grabbed::Withheld
        );
        assert_eq!(
            classify(Status::INVALIDATED_SESSION, &info).unwrap(),
            Grabbed::Lost
        );
        assert!(matches!(
            classify(Status::INVALID_TARGET, &info),
            Err(CaptureError::Unsupported(_))
        ));
        info.must_recreate = 1;
        assert_eq!(classify(Status::SUCCESS, &info).unwrap(), Grabbed::Lost);
        info.must_recreate = 0;
        info.height = 1440;
        assert_eq!(classify(Status::SUCCESS, &info).unwrap(), Grabbed::Resized);
    }

    #[test]
    fn a_reopen_waits_for_the_throttle_and_notes_how_it_went() {
        let mut throttle = ReopenThrottle::default();
        let now = Instant::now();
        let refused: Result<(), _> = Err(CaptureError::Nvfbc("step", Status::INVALIDATED_SESSION));
        assert!(matches!(
            note_reopened(&mut throttle, now, refused),
            Ok(None)
        ));
        assert!(throttle.attempt().is_none());
        assert!(matches!(
            note_reopened::<()>(&mut throttle, now, Err(CaptureError::Closed)),
            Err(CaptureError::Closed)
        ));
        assert!(matches!(
            note_reopened(&mut throttle, now, Ok(())),
            Ok(Some(()))
        ));
        assert!(throttle.attempt().is_some());
    }

    #[test]
    fn the_masked_key_and_its_mask_give_the_key() {
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
