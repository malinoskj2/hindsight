//! NvFBC capture of a monitor into CUDA device memory that NVENC reads in place on the same
//! CUDA context.

mod ffi;

use super::{
    Library, display_size, ffi as nvfbc_ffi, find_adapter, is_attached, key_size, nvfbc_failed,
    prepare, require_even, saved_key,
};
use crate::{
    gpu::{
        capture::{
            CaptureError, EncoderDevice, Frame, ReopenThrottle, Source, Surface, failed,
            is_window_closed, monitor_of,
        },
        cuda::{Context, CudaError, Device as CudaDevice, DevicePointer, Driver},
        device::Device,
    },
    log,
    service::Target,
};
use nvfbc_ffi::{Status, Zeroed};
use std::ptr::{self, NonNull};
use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        Direct3D9::{D3D_SDK_VERSION, Direct3DCreate9Ex},
        Direct3D11::ID3D11Texture2D,
        Gdi::HMONITOR,
    },
};

const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;
const BYTES_PER_PIXEL: u32 = 4;
const MEBIBYTE: u32 = 1024 * 1024;

pub(in crate::gpu::capture) struct Capture {
    /// Must outlive the session.
    _library: Library,
    create: nvfbc_ffi::CreateEx,
    key: Vec<u8>,
    /// Direct3D 9 adapter ordinal; the driver counts by it.
    adapter: u32,
    cuda: Driver,
    /// Current on this thread for the whole session.
    context: SessionContext,
    buffers: Vec<DevicePointer>,
    pitch: Option<u32>,
    session: Option<Session>,
    next: u32,
    handed_over: bool,
    size: (u32, u32),
    monitor: HMONITOR,
    window: Option<HWND>,
    reopen: ReopenThrottle,
    cursor: bool,
}

#[derive(Clone, Copy)]
enum SessionContext {
    Primary {
        device: CudaDevice,
        context: Context,
    },
    /// A context the driver opened for itself at session creation.
    Adopted(Context),
}

impl SessionContext {
    const fn handle(self) -> Context {
        match self {
            Self::Primary { context, .. } | Self::Adopted(context) => context,
        }
    }
}

enum Grab {
    Done(usize),
    /// Protected video is on screen.
    Withheld,
    /// The size or the row pitch changed.
    Resized,
    Lost,
}

impl Capture {
    pub(in crate::gpu::capture) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        let (monitor, window) = monitor_of(target);
        let key = saved_key();

        // Only to count adapters the way the driver does.
        // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
        let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(failed("start Direct3D 9 to find the monitor for NvFBC"))?;
        let adapter = find_adapter(&d3d9, device, monitor)?;
        let size = display_size(&d3d9, adapter)?;
        drop(d3d9);
        require_even(size, "NVIDIA NvFBC to CUDA")?;

        let library = Library::load()?;
        let create = prepare(&library, &key, adapter)?;
        let (cuda, cuda_device, context) = open_primary_context(device)?;

        let mut capture = Self {
            _library: library,
            create,
            key,
            adapter,
            cuda,
            context: SessionContext::Primary {
                device: cuda_device,
                context,
            },
            buffers: Vec::with_capacity(SLOTS),
            pitch: None,
            session: None,
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

    fn open_session(&mut self) -> Result<Session, CaptureError> {
        // Released by `Drop` should a later step fail.
        let session = Session {
            object: self
                .create_object()
                .map_err(nvfbc_failed("create the NvFBC session"))?,
        };
        self.settle_context()?;
        // Block rather than spin while a grab lands.
        self.cuda.block_on_synchronize().map_err(cuda_failed(
            "let CUDA block rather than spin while a grab lands",
        ))?;
        if self.buffers.is_empty() {
            self.allocate_buffers(&session)?;
        }
        session
            .set_up()
            .map_err(nvfbc_failed("set the NvFBC session up"))?;
        Ok(session)
    }

    /// No device or context is passed: the header ties the context parameter to a Direct3D 9
    /// device.
    fn create_object(&self) -> Result<NonNull<ffi::ToCuda>, Status> {
        let mut params = nvfbc_ffi::CreateParams::zeroed();
        params.version = nvfbc_ffi::CREATE_PARAMS_VER;
        params.interface_type = ffi::SHARED_CUDA;
        params.adapter_index = self.adapter;
        params.private_data = self.key.as_ptr().cast();
        params.private_data_size = key_size(&self.key);
        // SAFETY: the parameters are complete and the key outlives the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        NonNull::new(params.nvfbc.cast::<ffi::ToCuda>()).ok_or(Status::UNSUPPORTED)
    }

    /// Adopts a context of the driver's own the first time; after that the context must be the
    /// one the buffers and the encoder are on.
    fn settle_context(&mut self) -> Result<(), CaptureError> {
        let current = self
            .cuda
            .current_context()
            .map_err(cuda_failed("read the current CUDA context"))?;
        match current {
            Some(current) if current == self.context.handle() => Ok(()),
            Some(made) if self.buffers.is_empty() => self.adopt(made),
            _ => Err(CaptureError::Restart(
                "the NVIDIA driver opened the new NvFBC session on another CUDA context than the encoder's",
            )),
        }
    }

    fn adopt(&mut self, made: Context) -> Result<(), CaptureError> {
        let SessionContext::Primary { device, .. } = self.context else {
            return Ok(());
        };
        let step = cuda_failed("adopt the NvFBC driver's CUDA context");
        // The stack is the primary context with the driver's on top.
        self.cuda.pop_context().map_err(&step)?;
        self.cuda.pop_context().map_err(&step)?;
        self.cuda.push_context(made).map_err(&step)?;
        self.context = SessionContext::Adopted(made);
        self.cuda.release_primary_context(device).map_err(&step)?;
        log::info(
            "NvFBC opened a CUDA context of its own for the capture; the encoder runs on it too",
        );
        Ok(())
    }

    fn allocate_buffers(&mut self, session: &Session) -> Result<(), CaptureError> {
        let bytes = session
            .max_buffer_size()
            .map_err(nvfbc_failed("size the NvFBC buffers"))?;
        for _ in 0..SLOTS {
            let buffer = self
                .cuda
                .allocate(bytes as usize)
                .map_err(cuda_failed("allocate an NvFBC buffer"))?;
            self.buffers.push(buffer);
        }
        log::info(format!(
            "NvFBC grabs into {SLOT_COUNT} CUDA buffers of {} MiB",
            bytes.div_ceil(MEBIBYTE)
        ));
        Ok(())
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
            Err(CaptureError::Nvfbc(..)) => self.reopen.refused(now),
            Err(error) => return Err(error),
        }
        Ok(Frame::Unchanged)
    }

    fn grab(&mut self) -> Result<Grab, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grab::Lost);
        };
        let slot = self.next as usize;
        let (status, info) = session.grab_frame(self.buffers[slot], self.cursor);
        match status {
            Status::SUCCESS if info.must_recreate != 0 => Ok(Grab::Lost),
            Status::SUCCESS if (info.width, info.height) != self.size => Ok(Grab::Resized),
            Status::SUCCESS => {
                if !self.note_pitch(&info) {
                    return Ok(Grab::Resized);
                }
                // The grab only queues the copy; NVENC must not read the buffer before it lands.
                self.cuda
                    .synchronize_context()
                    .map_err(cuda_failed("wait for an NvFBC grab to land"))?;
                self.next = (self.next + 1) % SLOT_COUNT;
                Ok(Grab::Done(slot))
            }
            Status::INVALIDATED_SESSION => Ok(Grab::Lost),
            Status::PROTECTED_CONTENT => Ok(Grab::Withheld),
            status => Err(nvfbc_failed("grab the display with NvFBC into CUDA memory")(status)),
        }
    }

    /// False once the pitch changes: the encoder must then start over.
    fn note_pitch(&mut self, info: &nvfbc_ffi::FrameGrabInfo) -> bool {
        // A driver that reports no padded width pads nothing.
        let buffer_width = if info.buffer_width == 0 {
            info.width
        } else {
            info.buffer_width
        };
        let pitch = buffer_width * BYTES_PER_PIXEL;
        match self.pitch {
            None => {
                log::info(format!(
                    "NvFBC grabs {}×{} ARGB with a pitch of {pitch} bytes",
                    info.width, info.height
                ));
                self.pitch = Some(pitch);
                true
            }
            Some(known) => known == pitch,
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
            // CUDA memory cannot be copied into a Direct3D texture here.
            return Err(CaptureError::Unsupported(
                "NvFBC to CUDA can only encode its grabs in place".to_owned(),
            ));
        }
        if self.session.is_none() {
            return self.reopen();
        }
        match self.grab()? {
            Grab::Done(index) => Ok(Frame::InSurface(index)),
            Grab::Withheld => Ok(Frame::Unchanged),
            Grab::Resized => Ok(Frame::Resized),
            Grab::Lost => {
                self.session = None;
                self.reopen()
            }
        }
    }

    fn hand_over_surfaces(&mut self) -> bool {
        self.handed_over = true;
        true
    }

    fn surface(&self, index: usize) -> Option<Surface> {
        let pointer = *self.buffers.get(index)?;
        let pitch = self.pitch?;
        Some(Surface::CudaBuffer { pointer, pitch })
    }

    fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Cuda {
            context: self.context.handle(),
            primary: match self.context {
                SessionContext::Primary { device, .. } => Some(device),
                SessionContext::Adopted(_) => None,
            },
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Buffers before the session, the session before the context.
        for buffer in self.buffers.drain(..) {
            let _ = self.cuda.free(buffer);
        }
        self.session = None;
        // The driver may have taken its own context off the thread with the session.
        if let Ok(Some(current)) = self.cuda.current_context()
            && current == self.context.handle()
        {
            let _ = self.cuda.pop_context();
        }
        if let SessionContext::Primary { device, .. } = self.context {
            let _ = self.cuda.release_primary_context(device);
        }
    }
}

struct Session {
    /// Live until `Drop`; its method table lives in the library.
    object: NonNull<ffi::ToCuda>,
}

impl Session {
    fn methods(&self) -> &ffi::ToCudaVtable {
        // SAFETY: the object is live and the library holding its table is loaded.
        unsafe { &*(*self.object.as_ptr()).vtable }
    }

    /// For any display mode the adapter may switch to.
    fn max_buffer_size(&self) -> Result<u32, Status> {
        let mut bytes = 0;
        // SAFETY: a live object; the out-parameter outlives the call.
        unsafe { (self.methods().get_max_buffer_size)(self.object.as_ptr(), &raw mut bytes) }
            .ok()?;
        if bytes == 0 {
            return Err(Status::UNSUPPORTED);
        }
        Ok(bytes)
    }

    fn set_up(&self) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::SETUP_PARAMS_VER;
        setup.format = ffi::TOCUDA_ARGB;
        // SAFETY: a live object and a complete parameter structure.
        unsafe { (self.methods().set_up)(self.object.as_ptr(), &raw mut setup) }.ok()
    }

    /// The copy may still be under way when this returns.
    fn grab_frame(
        &self,
        buffer: DevicePointer,
        cursor: bool,
    ) -> (Status, nvfbc_ffi::FrameGrabInfo) {
        let mut info = nvfbc_ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::GRAB_FRAME_PARAMS_VER;
        // Not `CPU_SYNC`: the driver fails such grabs with `NVFBC_ERROR_NVAPI_FAILURE`;
        // `Capture::grab` waits for the copy instead.
        params.flags = ffi::TOCUDA_NOWAIT
            | if cursor {
                ffi::TOCUDA_WITH_HW_CURSOR
            } else {
                0
            };
        // A device address, not a pointer into this process.
        params.cuda_device_buffer = ptr::without_provenance_mut(
            usize::try_from(buffer).expect("a device address fits a 64-bit pointer"),
        );
        params.grab_info = &raw mut info;
        // SAFETY: a live, set-up object; the buffer is the driver's maximum size and `info`
        // outlives the call.
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

fn open_primary_context(device: &Device) -> Result<(Driver, CudaDevice, Context), CaptureError> {
    let cuda = Driver::load().map_err(cuda_failed("load the CUDA driver"))?;
    let cuda_device = pick_device(&cuda, device)?;
    let version = cuda
        .version()
        .map_err(cuda_failed("read the CUDA driver version"))?;
    log::info(format!(
        "NvFBC grabs into CUDA memory on CUDA device {cuda_device}, driver API {}.{}",
        version / 1000,
        version % 1000 / 10
    ));
    let context = cuda
        .retain_primary_context(cuda_device)
        .map_err(cuda_failed("open the CUDA context"))?;
    // `Capture`'s `Drop` pops it again.
    if let Err(error) = cuda.push_context(context) {
        let _ = cuda.release_primary_context(cuda_device);
        return Err(cuda_failed("make the CUDA context current")(error));
    }
    Ok((cuda, cuda_device, context))
}

fn pick_device(cuda: &Driver, device: &Device) -> Result<CudaDevice, CaptureError> {
    let luid = device.luid().map_err(failed("identify the encoding GPU"))?;
    let matched = cuda
        .device_with_luid(luid)
        .map_err(cuda_failed("find the encoding GPU's CUDA device"))?;
    if let Some(matched) = matched {
        return Ok(matched);
    }
    log::warning(format!(
        "No CUDA device reports the LUID of {}; using CUDA device 0 for NvFBC",
        device.adapter
    ));
    cuda.first_device()
        .map_err(cuda_failed("open CUDA device 0"))
}

fn cuda_failed(step: &'static str) -> impl Fn(CudaError) -> CaptureError {
    move |error| match error {
        CudaError::Library | CudaError::MissingEntryPoint(_) => {
            CaptureError::Unsupported(error.to_string())
        }
        CudaError::Call(..) => CaptureError::Cuda(step, error),
    }
}
