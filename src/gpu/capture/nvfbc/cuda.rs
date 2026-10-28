//! NvFBC capture of a monitor into CUDA device memory that NVENC reads in place on the same
//! CUDA context.

mod ffi;

use super::{
    Grabbed, KEY_BYTES, Key, Library, Object, Prepared, SLOT_COUNT, SLOTS, classify_grab,
    ffi as nvfbc_ffi, note_reopened, nvfbc_failed, prepare_capture, reopen_due,
};
use crate::{
    gpu::{
        capture::{
            CaptureError, CopyTarget, EncoderDevice, Frame, ReopenThrottle, Source, Surface, failed,
        },
        cuda::{Context, ContextHold, CudaError, Device as CudaDevice, DeviceBuffer, Driver},
        device::Device,
    },
    log,
    service::Target,
};
use bytemuck::Zeroable;
use nvfbc_ffi::Status;
use std::{marker::PhantomData, sync::Arc};
use windows::Win32::Graphics::Gdi::HMONITOR;

const BYTES_PER_PIXEL: u32 = 4;
const MEBIBYTE: u32 = 1024 * 1024;

pub(in crate::gpu::capture) struct Capture {
    create: nvfbc_ffi::CreateEx,
    key: Key,
    /// The Direct3D 9 adapter ordinal, which the driver counts by.
    adapter: u32,
    /// Current on this thread for the whole session.
    cuda: Arc<ContextHold>,
    /// Shared with the encoder, which reads them in place.
    buffers: Vec<Arc<DeviceBuffer>>,
    pitch: Option<u32>,
    session: Option<Session>,
    next: u32,
    handed_over: bool,
    size: (u32, u32),
    monitor: HMONITOR,
    reopen: ReopenThrottle,
    cursor: bool,
    /// The context is current on the thread that started the capture, and the context stack is
    /// per thread, so the capture stays on that thread.
    _thread: PhantomData<*const ()>,
    /// Declared last: `create` points into it. Each session holds it too.
    library: Arc<Library>,
}

impl Capture {
    pub(in crate::gpu::capture) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        // Direct3D 9 is opened only to count adapters the way the driver does.
        let Prepared {
            library,
            create,
            key,
            adapter,
            d3d9: _,
            size,
            monitor,
            set_global_flags: _,
        } = prepare_capture(device, target, "NVIDIA NvFBC to CUDA")?;
        let cuda = Arc::new(open_primary_context(device)?);

        let mut capture = Self {
            create,
            key,
            adapter,
            cuda,
            buffers: Vec::with_capacity(SLOTS),
            pitch: None,
            session: None,
            next: 0,
            handed_over: false,
            size,
            monitor,
            reopen: ReopenThrottle::default(),
            cursor,
            _thread: PhantomData,
            library,
        };
        capture.session = Some(capture.open_session()?);
        Ok(capture)
    }

    fn open_session(&mut self) -> Result<Session, CaptureError> {
        // Released by `Drop` if a later step fails.
        let session = Session {
            object: self
                .create_object()
                .map_err(nvfbc_failed("create the NvFBC session"))?,
        };
        self.settle_context()?;
        // Block rather than spin until the grab completes.
        self.cuda
            .driver()
            .block_on_synchronize()
            .map_err(cuda_failed(
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
    fn create_object(&self) -> Result<Object<ffi::ToCudaVtable>, Status> {
        let mut params = nvfbc_ffi::CreateParams::zeroed();
        params.version = nvfbc_ffi::CREATE_PARAMS_VER;
        params.interface_type = ffi::SHARED_CUDA;
        params.adapter_index = self.adapter;
        params.private_data = self.key.as_ptr().cast();
        params.private_data_size = KEY_BYTES;
        // SAFETY: the parameters are complete and the key outlives the call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        Object::new(params.nvfbc, &self.library).ok_or(Status::UNSUPPORTED)
    }

    /// Adopts the driver's own context the first time. After that, the context must be the one
    /// the buffers and the encoder are on.
    fn settle_context(&mut self) -> Result<(), CaptureError> {
        let current = self
            .cuda
            .driver()
            .current_context()
            .map_err(cuda_failed("read the current CUDA context"))?;
        match current {
            Some(current) if current == self.cuda.context() => Ok(()),
            Some(made) if self.buffers.is_empty() => self.adopt(made),
            _ => Err(CaptureError::Restart(
                "the NVIDIA driver opened the new NvFBC session on another CUDA context than the encoder's",
            )),
        }
    }

    fn adopt(&mut self, made: Context) -> Result<(), CaptureError> {
        // Only memory allocated in the context shares the hold, and there is none yet.
        let Some(hold) = Arc::get_mut(&mut self.cuda) else {
            return Err(CaptureError::Restart(
                "the CUDA context is in use and cannot give way to the NvFBC driver's",
            ));
        };
        if hold.primary().is_none() {
            return Ok(());
        }
        let step = cuda_failed("adopt the NvFBC driver's CUDA context");
        let driver = hold.driver();
        // The stack is the primary context with the driver's on top.
        driver.pop_context().map_err(&step)?;
        driver.pop_context().map_err(&step)?;
        driver.push_context(made).map_err(&step)?;
        hold.adopt(made).map_err(&step)?;
        log::info("NvFBC · own CUDA context · encoder runs on it");
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
            self.buffers.push(Arc::new(buffer));
        }
        log::info(format!(
            "NvFBC · {SLOT_COUNT} CUDA buffers of {} MiB",
            bytes.div_ceil(MEBIBYTE)
        ));
        Ok(())
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

    /// Grabs a frame into `slot`, reporting `Grabbed::Resized` also when the row pitch changed.
    fn grab(&mut self, slot: usize) -> Result<Grabbed, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grabbed::Lost);
        };
        let (status, info) = session.grab_frame(&self.buffers[slot], self.cursor);
        let grabbed = classify_grab(
            status,
            &info,
            self.size,
            "grab the display with NvFBC into CUDA memory",
        )?;
        if grabbed != Grabbed::Frame {
            return Ok(grabbed);
        }
        if !self.note_pitch(&info) {
            return Ok(Grabbed::Resized);
        }
        // The grab only queues the copy. NVENC must not read the buffer before the copy completes.
        self.cuda
            .driver()
            .synchronize_context()
            .map_err(cuda_failed("wait for an NvFBC grab to land"))?;
        self.next = (self.next + 1) % SLOT_COUNT;
        Ok(Grabbed::Frame)
    }

    /// Records the row pitch. Returns false once the pitch changes, because the encoder must then
    /// start over.
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
                    "NvFBC · {}×{} ARGB · pitch {pitch} bytes",
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

    fn take(&mut self, _target: Option<CopyTarget<'_>>) -> Result<Frame, CaptureError> {
        if !self.handed_over {
            // CUDA memory cannot be copied into a Direct3D texture here.
            return Err(CaptureError::Unsupported(
                "NvFBC to CUDA can only encode its grabs in place".to_owned(),
            ));
        }
        if self.session.is_none() {
            return self.reopen();
        }
        let slot = self.next as usize;
        match self.grab(slot)? {
            Grabbed::Frame => self.in_surface(slot),
            Grabbed::Withheld => Ok(Frame::Unchanged),
            Grabbed::Resized => Ok(Frame::Resized),
            Grabbed::Lost => {
                self.session = None;
                self.reopen()
            }
        }
    }
}

impl Capture {
    pub(in crate::gpu::capture) fn hand_over_surfaces(&mut self) -> bool {
        self.handed_over = true;
        true
    }

    /// Returns the frame for a grab into `slot`'s buffer, whose pitch `note_pitch` recorded.
    fn in_surface(&self, slot: usize) -> Result<Frame, CaptureError> {
        let pitch = self.pitch.ok_or(CaptureError::Restart(
            "NvFBC delivered a grab without its pitch",
        ))?;
        Ok(Frame::InSurface {
            index: slot,
            surface: Surface::CudaBuffer {
                buffer: Arc::clone(&self.buffers[slot]),
                pitch,
            },
        })
    }

    pub(in crate::gpu::capture) fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    pub(in crate::gpu::capture) fn encoder_device(&self) -> EncoderDevice {
        EncoderDevice::Cuda {
            hold: Arc::clone(&self.cuda),
            ring: None,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // The buffers are dropped before the session, both while the context is still on this
        // thread's stack. The hold releases the context after this, and the library is dropped
        // last of all.
        self.buffers.clear();
        self.session = None;
        let driver = self.cuda.driver();
        // The driver may have taken its own context off the thread with the session.
        if let Ok(Some(current)) = driver.current_context()
            && current == self.cuda.context()
        {
            let _ = driver.pop_context();
        }
    }
}

struct Session {
    object: Object<ffi::ToCudaVtable>,
}

impl Session {
    fn methods(&self) -> &ffi::ToCudaVtable {
        self.object.methods()
    }

    /// Returns the buffer size needed for any display mode the adapter may switch to.
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
        buffer: &DeviceBuffer,
        cursor: bool,
    ) -> (Status, nvfbc_ffi::FrameGrabInfo) {
        let mut info = nvfbc_ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::GRAB_FRAME_PARAMS_VER;
        // Not `CPU_SYNC`: the driver fails such grabs with `NVFBC_ERROR_NVAPI_FAILURE`, so
        // `Capture::grab` waits for the copy instead.
        params.flags = ffi::TOCUDA_NOWAIT
            | if cursor {
                ffi::TOCUDA_WITH_HW_CURSOR
            } else {
                0
            };
        params.cuda_device_buffer = buffer.pointer().as_api_pointer();
        params.grab_info = &raw mut info;
        // SAFETY: a live, set-up object; the buffer is the driver's maximum size and `info`
        // outlives the call.
        let status = unsafe { (self.methods().grab_frame)(self.object.as_ptr(), &raw mut params) };
        (status, info)
    }
}

fn open_primary_context(device: &Device) -> Result<ContextHold, CaptureError> {
    let cuda = Driver::load().map_err(cuda_failed("load the CUDA driver"))?;
    let cuda_device = pick_device(&cuda, device)?;
    let version = cuda
        .version()
        .map_err(cuda_failed("read the CUDA driver version"))?;
    log::info(format!(
        "NvFBC · CUDA device {cuda_device} · driver API {}.{}",
        version / 1000,
        version % 1000 / 10
    ));
    let hold = cuda
        .retain_primary_context(cuda_device)
        .map_err(cuda_failed("open the CUDA context"))?;
    // `Capture`'s `Drop` pops it again.
    hold.driver()
        .push_context(hold.context())
        .map_err(cuda_failed("make the CUDA context current"))?;
    Ok(hold)
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
        "NvFBC · no CUDA device matches {} · using device 0",
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
        CudaError::Call(..) | CudaError::OutOfRange { .. } => CaptureError::Cuda(step, error),
    }
}
