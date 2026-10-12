//! NvFBC capture of a monitor into CUDA device memory, which NVENC reads in place from an
//! encode session opened on the same CUDA context. The path from the frame buffer to the
//! encoder has no Direct3D in it: no Direct3D 9 device, no shared surfaces, and no encoder
//! input textures either, since every grab is encoded where the driver left it.
//!
//! Three buffers the driver's maximum size, as many as the Direct3D 9 backend has surfaces,
//! are grabbed into in turn: the one grabbed into next was last encoded three ticks earlier,
//! which NVENC has almost always finished by then, and the pipeline waits for it when it has
//! not. A grab is asked to return only once the picture has landed, so a buffer is complete
//! when it is handed over. Every grab counts as a new frame, since the driver does not say
//! whether the screen changed.
//!
//! The CUDA context is the device's primary one, retained for the session; should the driver
//! prefer to open its own context for the capture, as it has been seen to do, that context is
//! adopted for the buffers and the encoder instead, and the primary one is given back. Which
//! happened is logged. The key, the adapter checks and the reopening of a lost session are the
//! Direct3D 9 backend's, in the parent module.

mod ffi;

use super::{
    Library, display_size, ffi as nvfbc_ffi, find_adapter, is_attached, key_size, nvfbc_failed,
    prepare, saved_key,
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

/// Buffers grabbed into in turn: one for the grab just handed over and two for the frames
/// NVENC may still be reading, as with the Direct3D 9 backend's surfaces.
const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;
/// Bytes per pixel of the ARGB the driver writes.
const BYTES_PER_PIXEL: u32 = 4;
/// Bytes in a mebibyte, for the log.
const MEBIBYTE: u32 = 1024 * 1024;

/// A running NvFBC capture of one monitor into CUDA memory.
pub(in crate::gpu::capture) struct Capture {
    /// Unloaded only after `Drop` has released the session, the one thing here that needs it.
    _library: Library,
    /// The library's entry point that creates a capture object.
    create: nvfbc_ffi::CreateEx,
    /// The key presented to the driver whenever a session is opened.
    key: Vec<u8>,
    /// The Direct3D 9 adapter ordinal of the captured monitor, which the driver counts by.
    adapter: u32,
    /// The CUDA driver, which outlives the context and the buffers.
    cuda: Driver,
    /// The context the buffers live in and the encoder runs on, current on this thread for
    /// the whole session.
    context: SessionContext,
    /// The buffers the driver grabs into, allocated with the first session; empty until then.
    buffers: Vec<DevicePointer>,
    /// The distance between rows of a grab in bytes, known from the first grab on.
    pitch: Option<u32>,
    /// `None` while the driver has taken it away.
    session: Option<Session>,
    /// The buffer the next grab goes into.
    next: u32,
    /// Whether the encoder reads the buffers in place, the only way they can be read.
    handed_over: bool,
    size: (u32, u32),
    /// The captured monitor, checked for before a lost session is opened again.
    monitor: HMONITOR,
    /// The window whose monitor is recorded, when the target is one.
    window: Option<HWND>,
    reopen: ReopenThrottle,
    /// Whether the driver draws the mouse cursor into each grab.
    cursor: bool,
}

/// Whose CUDA context the session runs in.
#[derive(Clone, Copy)]
enum SessionContext {
    /// The primary context of `device`, retained by this capture.
    Primary {
        device: CudaDevice,
        context: Context,
    },
    /// A context the NvFBC driver opened for itself while the session was created, which it
    /// owns and may close with the session.
    Adopted(Context),
}

impl SessionContext {
    const fn handle(self) -> Context {
        match self {
            Self::Primary { context, .. } | Self::Adopted(context) => context,
        }
    }
}

/// What a grab came to.
enum Grab {
    /// A new picture is complete in the buffer with this index.
    Done(usize),
    /// Protected video is on screen, and the driver withholds the picture.
    Withheld,
    /// The display's size, or the driver's row pitch, changed.
    Resized,
    /// The driver ended the session.
    Lost,
}

impl Capture {
    /// Starts capturing the monitor `target` names, or the monitor its window fills, with the
    /// cursor drawn in if `cursor` is set.
    pub(in crate::gpu::capture) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        let (monitor, window) = monitor_of(target);
        let key = saved_key()?;

        // Direct3D 9 is used for nothing but counting adapters the way the driver does; no
        // device is created on it.
        // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
        let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(failed("start Direct3D 9 to find the monitor for NvFBC"))?;
        let adapter = find_adapter(&d3d9, device, monitor)?;
        let size = display_size(&d3d9, adapter)?;
        drop(d3d9);
        require_even(size)?;

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

    /// Creates the driver's capture object, settles which CUDA context the session runs in,
    /// allocates the buffers the first time, and sets the capture up.
    fn open_session(&mut self) -> Result<Session, CaptureError> {
        // Released by `Drop` should a later step fail.
        let session = Session {
            object: self
                .create_object()
                .map_err(nvfbc_failed("create the NvFBC session"))?,
        };
        self.settle_context()?;
        if self.buffers.is_empty() {
            self.allocate_buffers(&session)?;
        }
        session
            .set_up()
            .map_err(nvfbc_failed("set the NvFBC session up"))?;
        Ok(session)
    }

    /// Asks the driver for a capture object on the adapter, presenting the key. No device and
    /// no context are passed: the header ties the context parameter to a Direct3D 9 device,
    /// which this backend exists to avoid.
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

    /// Checks which context the driver left current after creating the session. The first
    /// time, a context of the driver's own is adopted in place of the primary one; after that
    /// the context must be the one the buffers and the encoder are on, or the session cannot
    /// go on in place.
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

    /// Switches the session to `made`, the context the driver pushed over the primary one,
    /// and gives the primary one back.
    fn adopt(&mut self, made: Context) -> Result<(), CaptureError> {
        let SessionContext::Primary { device, .. } = self.context else {
            return Ok(());
        };
        let step = cuda_failed("adopt the NvFBC driver's CUDA context");
        // The stack holds the primary context with the driver's on top; leave only the
        // driver's.
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

    /// Allocates the buffers in the session's context, each as large as the driver may need
    /// for any display mode.
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

    /// Opens the session again after the driver ended it. While the driver refuses, the last
    /// picture is repeated; a monitor that went away ends the capture.
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

    /// Grabs the display into the next buffer and waits for it to land.
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
                self.next = (self.next + 1) % SLOT_COUNT;
                Ok(Grab::Done(slot))
            }
            Status::INVALIDATED_SESSION => Ok(Grab::Lost),
            Status::PROTECTED_CONTENT => Ok(Grab::Withheld),
            status => Err(nvfbc_failed("grab the display with NvFBC into CUDA memory")(status)),
        }
    }

    /// Records the row pitch of the grab `info` describes, and reports whether it is the one
    /// the buffers were registered with: a changed pitch means the encoder must start over.
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
            // CUDA memory cannot be copied into a Direct3D texture here, and the pipeline
            // hands surfaces over whenever the display's size is encodable, which `start`
            // made sure of.
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
        // The buffers go before the session, as in NVIDIA's sample, and the session before
        // the context, which the driver's object may hold on to. The encoder, which reads
        // the buffers, was dropped before the capture.
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
        // The fields then unload the NvFBC library, which nothing of the driver's is left in,
        // and the CUDA driver.
    }
}

/// A capture object of the driver's, released when dropped.
struct Session {
    /// Live from `NvFBC_CreateEx` until `Drop` releases it; its method table lives in the
    /// library, which outlives the session.
    object: NonNull<ffi::ToCuda>,
}

impl Session {
    fn methods(&self) -> &ffi::ToCudaVtable {
        // SAFETY: the object is live (see the field), and a live object's table pointer is
        // valid for as long as the library that made it is loaded.
        unsafe { &*(*self.object.as_ptr()).vtable }
    }

    /// The most bytes a grab can take, for any display mode the adapter may switch to.
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

    /// Sets the session up to grab 32-bit ARGB, with the cursor left to each grab.
    fn set_up(&self) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::SETUP_PARAMS_VER;
        setup.format = ffi::TOCUDA_ARGB;
        // SAFETY: a live object and a complete parameter structure.
        unsafe { (self.methods().set_up)(self.object.as_ptr(), &raw mut setup) }.ok()
    }

    /// Grabs the whole display into `buffer` without waiting for the screen to change, with
    /// the cursor drawn in if `cursor` is set, returning once the picture has landed, and
    /// returns the driver's status with its report on the grab.
    fn grab_frame(
        &self,
        buffer: DevicePointer,
        cursor: bool,
    ) -> (Status, nvfbc_ffi::FrameGrabInfo) {
        let mut info = nvfbc_ffi::FrameGrabInfo::zeroed();
        let mut params = ffi::GrabFrameParams::zeroed();
        params.version = ffi::GRAB_FRAME_PARAMS_VER;
        params.flags = ffi::TOCUDA_NOWAIT
            | ffi::TOCUDA_CPU_SYNC
            | if cursor {
                ffi::TOCUDA_WITH_HW_CURSOR
            } else {
                0
            };
        // A device address is not a pointer into this process; the header merely types it
        // as one.
        params.cuda_device_buffer = ptr::without_provenance_mut(
            usize::try_from(buffer).expect("a device address fits a 64-bit pointer"),
        );
        params.grab_info = &raw mut info;
        // SAFETY: a live object, set up by `Capture::open_session` before it handed the
        // session out; the buffer is the driver's maximum size and `info` outlives the call.
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

/// Refuses a display with an odd width or height: NVENC encodes even sizes, and a grab is
/// read in place, uncropped.
fn require_even((width, height): (u32, u32)) -> Result<(), CaptureError> {
    if width % 2 == 0 && height % 2 == 0 {
        return Ok(());
    }
    Err(CaptureError::Unsupported(format!(
        "the display is {width}×{height}; NvFBC to CUDA encodes its grabs in place and needs an even width and height, so choose NVIDIA NvFBC"
    )))
}

/// The CUDA driver with the encoding GPU's primary context retained and current on this
/// thread, for the session to run in; which device and driver were found is logged.
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
    // Current for the rest of the session; `Capture`'s `Drop` takes it off again. Until the
    // capture holds it, nothing else would give the retained context back.
    if let Err(error) = cuda.push_context(context) {
        let _ = cuda.release_primary_context(cuda_device);
        return Err(cuda_failed("make the CUDA context current")(error));
    }
    Ok((cuda, cuda_device, context))
}

/// The CUDA device of the encoding GPU, found by the adapter LUID both APIs report, or the
/// first device with a warning when none matches.
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

/// Wraps a CUDA error with the step that failed, for `map_err`; a missing library or entry
/// point reads as unsupported, since trying again would not change it.
fn cuda_failed(step: &'static str) -> impl Fn(CudaError) -> CaptureError {
    move |error| match error {
        CudaError::Library | CudaError::MissingEntryPoint(_) => {
            CaptureError::Unsupported(error.to_string())
        }
        CudaError::Call(..) => CaptureError::Cuda(step, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_displays_are_refused() {
        assert!(require_even((2560, 1440)).is_ok());
        assert!(matches!(
            require_even((1367, 768)),
            Err(CaptureError::Unsupported(_))
        ));
        assert!(matches!(
            require_even((1366, 767)),
            Err(CaptureError::Unsupported(_))
        ));
    }
}
