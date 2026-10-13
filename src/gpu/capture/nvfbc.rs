//! NVIDIA Frame Buffer Capture of a monitor into Direct3D 9 surfaces that NVENC reads in place
//! on the same device.

mod cuda;
mod ffi;

use super::{
    CaptureError, CopyTarget, EncoderDevice, Frame, ReopenThrottle, Source, Surface, failed,
    is_window_closed, monitor_of,
};
use crate::{gpu::device::Device, log, service::Target};
use std::{
    ffi::c_void,
    ptr::{self, NonNull},
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
        UI::WindowsAndMessaging::GetDesktopWindow,
    },
    core::{BOOL, Interface, s, w},
};

use crate::gpu::ffi::{Library, Symbol, Zeroed};
pub(super) use cuda::Capture as CudaCapture;
pub(in crate::gpu) use ffi::Status;

/// The most the driver accepts; it refuses four.
const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;

pub(super) struct Capture {
    /// Must outlive the session.
    _library: Library,
    create: ffi::CreateEx,
    key: Key,
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
            window,
        } = prepare_capture(device, target, "NVIDIA NvFBC")?;
        let d3d9_device = create_device(&d3d9, adapter)?;
        let slots = [
            create_slot(&d3d9_device, size)?,
            create_slot(&d3d9_device, size)?,
            create_slot(&d3d9_device, size)?,
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
            .set_up(self.cursor)
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
        // SAFETY: the device and the key outlive the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        Object::new(params.nvfbc).ok_or(Status::UNSUPPORTED)
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
            // No wait: NVENC reads on the same device, which orders the blit first.
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
            Grabbed::Frame | Grabbed::Withheld => {}
            Grabbed::Resized => return Ok(Frame::Resized),
            // Deliver the collected frame first; reopen on the next tick.
            Grabbed::Lost if collected.is_some() => self.session = None,
            Grabbed::Lost => {
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
        Some(Surface::Direct3D9(slot.surface.clone()))
    }

    fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Direct3D9(self.device.clone())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // The session goes before the library that implements it.
        self.session = None;
    }
}

/// What both backends need before their first session.
struct Prepared {
    library: Library,
    create: ffi::CreateEx,
    key: Key,
    /// Direct3D 9 adapter ordinal; the driver counts by it.
    adapter: u32,
    d3d9: IDirect3D9Ex,
    size: (u32, u32),
    monitor: HMONITOR,
    window: Option<HWND>,
}

/// Finds the monitor on the encoding GPU, loads the library and turns NvFBC on if need be.
fn prepare_capture(
    device: &Device,
    target: &Target,
    backend: &str,
) -> Result<Prepared, CaptureError> {
    let (monitor, window) = monitor_of(target);
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
    Ok(Prepared {
        library,
        create,
        key,
        adapter,
        d3d9,
        size,
        monitor,
        window,
    })
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
        Ok(Self {
            get_status,
            create,
            enable,
        })
    }
}

/// A live interface object from `NvFBC_CreateEx`, released on drop.
struct Object<V: ffi::Vtable>(NonNull<ffi::Interface<V>>);

impl<V: ffi::Vtable> Object<V> {
    fn new(pointer: *mut c_void) -> Option<Self> {
        NonNull::new(pointer.cast()).map(Self)
    }

    fn as_ptr(&self) -> *mut ffi::Interface<V> {
        self.0.as_ptr()
    }

    fn methods(&self) -> &V {
        // SAFETY: the object is live and the library holding its table is loaded.
        unsafe { &*(*self.0.as_ptr()).vtable }
    }
}

impl<V: ffi::Vtable> Drop for Object<V> {
    fn drop(&mut self) {
        // SAFETY: a live object from `NvFBC_CreateEx`; nothing uses it afterwards.
        let _ = unsafe { (self.methods().release())(self.0.as_ptr()) };
    }
}

struct Slot {
    surface: IDirect3DSurface9,
    /// Keeps the surface's parent texture alive.
    _texture: IDirect3DTexture9,
}

struct Session {
    object: Object<ffi::ToDx9VidVtable>,
    /// The driver may read these for the session's life.
    buffers: Box<[ffi::OutputBuffer; SLOTS]>,
}

impl Session {
    fn set_up(&self, cursor: bool) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::TODX9VID_SETUP_PARAMS_VER;
        setup.flags = if cursor { ffi::SETUP_WITH_HW_CURSOR } else { 0 };
        setup.mode = ffi::TODX9VID_ARGB;
        setup.buffer_count = SLOT_COUNT;
        setup.buffers = self.buffers.as_ptr().cast_mut();
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
    /// The picture is in the buffer asked for.
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

/// `Some(now)` when a lost session may be opened again; a monitor that is gone ends the capture.
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

/// A passing refusal holds the next attempt back; permanent ones already read as unsupported.
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

// The driver's key, masked in the binary and unmasked at use.
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

fn create_slot(d3d9: &IDirect3DDevice9Ex, size: (u32, u32)) -> Result<Slot, CaptureError> {
    let texture = create_texture(d3d9, size)?;
    // SAFETY: the texture has one level.
    let surface =
        unsafe { texture.GetSurfaceLevel(0) }.map_err(failed("open the NvFBC surface"))?;
    Ok(Slot {
        surface,
        _texture: texture,
    })
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
