//! NVIDIA Frame Buffer Capture (NvFBC) of a monitor, the capture Instant Replay itself uses.
//! The driver copies the display, cursor included, straight from the frame buffer it scans
//! out, so it costs a game nothing that Windows composes.
//!
//! NvFBC on Windows writes into Direct3D 9 surfaces. Three, as many as the driver takes, are
//! created on a Direct3D 9Ex device on the monitor's GPU, shared with the encoder's Direct3D 11
//! device: on each tick the grab the previous tick started is handed to the encoder, and the
//! next grab goes into the next surface, so the capture thread never waits for the GPU. NVENC
//! reads the surfaces in place once they are handed over; the surface the next grab goes into
//! was last encoded two ticks earlier, which NVENC has almost always finished by then, and is
//! waited for when it has not. Otherwise each grab is copied into the encoder's input texture.
//! Every grab counts as a new frame, since the driver does not say whether the screen changed.
//!
//! The driver only lets a licensed application capture with NvFBC on a GeForce GPU: it must
//! present the key NVIDIA issued, which is entered in Settings and read from the registry. NvFBC
//! must also be turned on in the driver, which takes administrator rights once.
//!
//! Like Desktop Duplication, NvFBC sees whole monitors connected to the GPU, so a window target
//! is recorded through its monitor. When the driver ends the session (a mode change, the
//! secure desktop) it is opened again in place, keeping the buffer, unless the size changed.
//!
//! Four environment variables, read once when a capture starts and meant for measuring
//! against Instant Replay with the benchmark harness (`docs/nvfbc-source.md`), change what the
//! driver is asked for; unset, the capture behaves exactly as described above.
//! `HINDSIGHT_NVFBC_PROCESS=<pid>` asks for that process's presented frames instead of the
//! display's scanned-out frame, through the private structure Instant Replay itself passes (the
//! driver may refuse it, and the process must present at the display's size).
//! `HINDSIGHT_NVFBC_GRAB_MODE=scale|crop` grabs through the driver's scaling or cropping path
//! at the display's own size instead of the plain full-size path. `HINDSIGHT_NVFBC_WAIT_TIMEOUT=<ms>`
//! has each grab wait up to that long for a new picture instead of taking the current one.
//! While any of these three is set, the first grab's report (size, source process, wait mode) is
//! logged.
//!
//! `HINDSIGHT_NVFBC_FORMAT=nv12` has the driver grab NV12, as Instant Replay does, instead of
//! ARGB. Direct3D 9 makes NV12 only as offscreen plain surfaces, which Direct3D 11 cannot open
//! (nor can Direct3D 9 open an NV12 texture Direct3D 11 shares), so the surfaces are not shared:
//! NVENC runs on this capture's Direct3D 9Ex device and reads them there, which needs a display
//! of even width and height, and a grab can never be copied. The driver converts with the
//! BT.601 matrix, which the clip then signals (`docs/backend-overhead.md`, "NV12 from NvFBC").
//! The device is used from the capture thread only, by the grabs and by the encoder alike.
//!
//! `HINDSIGHT_NVFBC_ENCODER=d3d9` has NVENC read the ARGB grabs the same way, on this capture's
//! Direct3D 9Ex device, as NVIDIA's `NvFBCDX9NvEnc` sample does, instead of through the shared
//! textures on the Direct3D 11 device. The picture and the stream stay as they are (ARGB
//! converted by NVENC, BT.709), and with one device reading and writing the surfaces the grab
//! needs no completion query (`HINDSIGHT_GRAB_SYNC=none`). It too needs an even display.
//!
//! How each grab is known to have landed before NVENC reads its surface, and the switches that
//! compare the ways of doing it (`HINDSIGHT_GRAB_SYNC`, `HINDSIGHT_GRAB_SYNC_STATS`), are
//! described in [`sync`].

mod cuda;
mod ffi;
mod sync;
#[cfg(test)]
mod sync_cost;

use super::{
    CaptureError, EncoderDevice, Frame, ReopenThrottle, Source, Surface, failed, is_window_closed,
    monitor_of,
};
use crate::{gpu::device::Device, log, replay::Matrix, service::Target, sys::nvfbc_key};
use std::ptr::{self, NonNull};
use sync::{Call, GrabSync, Stats, timed};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HANDLE, HMODULE, HWND, LUID},
        Graphics::{
            Direct3D9::{
                D3D_SDK_VERSION, D3DCREATE_FPU_PRESERVE, D3DCREATE_MULTITHREADED,
                D3DCREATE_NOWINDOWCHANGES, D3DCREATE_SOFTWARE_VERTEXPROCESSING, D3DDEVTYPE_HAL,
                D3DDISPLAYMODEEX, D3DDISPLAYROTATION, D3DDISPLAYROTATION_IDENTITY, D3DFMT_A8R8G8B8,
                D3DFMT_UNKNOWN, D3DFORMAT, D3DPOOL_DEFAULT, D3DPRESENT_INTERVAL_IMMEDIATE,
                D3DPRESENT_PARAMETERS, D3DQUERYTYPE_EVENT, D3DSWAPEFFECT_DISCARD,
                D3DUSAGE_RENDERTARGET, Direct3DCreate9Ex, IDirect3D9Ex, IDirect3DDevice9Ex,
                IDirect3DQuery9, IDirect3DSurface9, IDirect3DTexture9,
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
/// The same capture into CUDA memory, for an encoder on a CUDA context.
pub(super) use cuda::Capture as CudaCapture;
pub(in crate::gpu) use ffi::Status;
use ffi::Zeroed;

/// Surfaces grabbed into in turn: the most the driver accepts (it refuses four), one for the
/// grab on its way and two for the frames NVENC may still be reading.
const SLOT_COUNT: u32 = 3;
const SLOTS: usize = SLOT_COUNT as usize;
/// The variable naming the process whose frames to grab; see [`Switches`].
const PROCESS_VARIABLE: &str = "HINDSIGHT_NVFBC_PROCESS";
/// The variable naming the driver's grab path; see [`Switches`].
const GRAB_MODE_VARIABLE: &str = "HINDSIGHT_NVFBC_GRAB_MODE";
/// The variable giving how long a grab waits for a new picture; see [`Switches`].
const WAIT_TIMEOUT_VARIABLE: &str = "HINDSIGHT_NVFBC_WAIT_TIMEOUT";
/// The variable naming the pixel format the driver grabs into; see [`PixelFormat`].
const FORMAT_VARIABLE: &str = "HINDSIGHT_NVFBC_FORMAT";
/// The variable naming the device NVENC reads ARGB grabs on; see [`EncoderOn`].
const ENCODER_VARIABLE: &str = "HINDSIGHT_NVFBC_ENCODER";
/// `MAKEFOURCC('N', 'V', '1', '2')`, which Direct3D 9 takes as a format although its headers
/// name no constant for it.
const D3DFMT_NV12: D3DFORMAT = D3DFORMAT(u32::from_le_bytes(*b"NV12"));

/// A running NvFBC capture of one monitor.
pub(super) struct Capture {
    /// Unloaded only after `Drop` has released the session, the one thing here that needs it.
    _library: Library,
    /// The library's entry point that creates a capture object.
    create: ffi::CreateEx,
    /// The key presented to the driver whenever a session is opened.
    key: Vec<u8>,
    /// The Direct3D 9Ex device the surfaces live on.
    device: IDirect3DDevice9Ex,
    slots: [Slot; SLOTS],
    /// `None` while the driver has taken it away.
    session: Option<Session>,
    /// The slot whose grab is on its way, to be copied on the next tick.
    pending: Option<usize>,
    /// The slot the next grab goes into.
    next: u32,
    /// Whether the encoder reads the surfaces in place, so a finished grab is not copied.
    handed_over: bool,
    size: (u32, u32),
    /// The captured monitor, checked for before a lost session is opened again.
    monitor: HMONITOR,
    /// The window whose monitor is recorded, when the target is one.
    window: Option<HWND>,
    reopen: ReopenThrottle,
    /// Whether the driver draws the mouse cursor into each grab.
    cursor: bool,
    /// The experimental settings this capture was started with.
    switches: Switches,
    /// Whether the first grab's report has been logged, which only a switch asks for.
    reported: bool,
    /// What the driver grabs into the surfaces.
    format: PixelFormat,
    /// The device NVENC reads the surfaces on: Direct3D 9 for NV12, and for ARGB when asked.
    encoder: EncoderOn,
    /// How a grab is known to have landed before its surface is handed over.
    sync: GrabSync,
    /// The cycles spent on each grab and its synchronisation, when they are counted.
    stats: Option<Stats>,
}

/// What a capture is started with besides its target: the settings the environment variables
/// named in the module documentation and in [`sync`] choose.
struct Setup {
    switches: Switches,
    format: PixelFormat,
    encoder: EncoderOn,
    /// The mode asked for, before [`GrabSync::effective`] checks it against the format.
    sync: GrabSync,
    stats: Option<Stats>,
}

impl Setup {
    /// What the environment asks for.
    fn from_environment() -> Self {
        Self {
            switches: Switches::from_environment(),
            format: PixelFormat::from_environment(),
            encoder: EncoderOn::from_environment(),
            sync: GrabSync::from_environment(),
            stats: Stats::from_environment(),
        }
    }
}

/// The pixel format the driver grabs into, which `HINDSIGHT_NVFBC_FORMAT` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelFormat {
    /// BGRA in textures shared with the Direct3D 11 device, the default.
    Argb,
    /// NV12 in plain surfaces only the Direct3D 9 device sees, converted with BT.601.
    Nv12,
}

impl PixelFormat {
    /// What the environment asks for.
    fn from_environment() -> Self {
        Self::parse(std::env::var(FORMAT_VARIABLE).ok().as_deref())
    }

    /// The format a value of the variable asks for, without regard to case or surrounding
    /// whitespace; anything but `nv12` leaves ARGB.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("nv12") => Self::Nv12,
            _ => Self::Argb,
        }
    }

    /// The driver's `NVFBC_TODX9VID_*` mode for the format.
    const fn mode(self) -> u32 {
        match self {
            Self::Argb => ffi::TODX9VID_ARGB,
            Self::Nv12 => ffi::TODX9VID_NV12,
        }
    }
}

/// The device NVENC reads ARGB grabs on, which `HINDSIGHT_NVFBC_ENCODER` chooses; NV12 grabs
/// are always read on the Direct3D 9Ex device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderOn {
    /// The shared Direct3D 11 device, through textures opened from the surfaces, the default.
    Direct3D11,
    /// The capture's own Direct3D 9Ex device, reading the surfaces themselves.
    Direct3D9,
}

impl EncoderOn {
    /// What the environment asks for.
    fn from_environment() -> Self {
        Self::parse(std::env::var(ENCODER_VARIABLE).ok().as_deref())
    }

    /// The device a value of the variable names, without regard to case or surrounding
    /// whitespace; anything but `d3d9` leaves Direct3D 11.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("d3d9") => Self::Direct3D9,
            _ => Self::Direct3D11,
        }
    }
}

/// What starting a grab came to.
enum Grab {
    /// A grab is on its way into the next slot.
    Started,
    /// Protected video is on screen, and the driver withholds the picture.
    Withheld,
    /// The display's size changed.
    Resized,
    /// The driver ended the session.
    Lost,
}

/// The experimental parameters the environment variables named in the module documentation
/// choose, read once when a capture starts. All unset, the default, is today's behaviour.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Switches {
    /// The process whose presented frames the session grabs instead of the display's.
    process: Option<u32>,
    /// Which of the driver's grab paths a frame takes.
    grab_mode: GrabMode,
    /// How many milliseconds a grab may wait for a new picture; `None` takes the current one.
    wait_timeout: Option<u32>,
}

/// `NVFBC_TODX9VID_SOURCEMODE_*`, each asked for at the display's own size.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrabMode {
    Full,
    Scale,
    Crop,
}

impl Switches {
    /// Today's behaviour.
    const DEFAULT: Self = Self {
        process: None,
        grab_mode: GrabMode::Full,
        wait_timeout: None,
    };

    /// What the environment asks for; a variable that is unset, empty or unreadable leaves
    /// its default.
    fn from_environment() -> Self {
        let variable = |name| std::env::var(name).ok();
        Self::parse(
            variable(PROCESS_VARIABLE).as_deref(),
            variable(GRAB_MODE_VARIABLE).as_deref(),
            variable(WAIT_TIMEOUT_VARIABLE).as_deref(),
        )
    }

    /// The switches the variables' values ask for, each read without regard to case or
    /// surrounding whitespace; a value that is missing, empty or malformed leaves its default.
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

    /// Writes the switches to the log when any is set, so a measurement's log says which arm
    /// it was.
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
    /// Starts capturing the monitor `target` names, or the monitor its window fills, with the
    /// cursor drawn in if `cursor` is set.
    pub(super) fn start(
        device: &Device,
        target: &Target,
        cursor: bool,
    ) -> Result<Self, CaptureError> {
        Self::start_with(device, target, cursor, Setup::from_environment())
    }

    /// Starts capturing as [`Capture::start`] does, with the settings `setup` gives.
    fn start_with(
        device: &Device,
        target: &Target,
        cursor: bool,
        setup: Setup,
    ) -> Result<Self, CaptureError> {
        let Setup {
            switches,
            format,
            encoder,
            sync,
            stats,
        } = setup;
        let encoder = if format == PixelFormat::Nv12 {
            EncoderOn::Direct3D9
        } else {
            encoder
        };
        let encodes_on_d3d9 = encoder == EncoderOn::Direct3D9;
        let (monitor, window) = monitor_of(target);
        let key = saved_key()?;
        switches.report();
        let effective = sync.effective(encodes_on_d3d9);
        GrabSync::report(sync, effective);

        // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
        let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(failed("start Direct3D 9 for NvFBC"))?;
        let adapter = find_adapter(&d3d9, device, monitor)?;
        let size = display_size(&d3d9, adapter)?;
        if encodes_on_d3d9 {
            require_even(size)?;
        }
        if format == PixelFormat::Nv12 {
            log::info(
                "NvFBC grabs NV12 (HINDSIGHT_NVFBC_FORMAT=nv12): NVENC runs on NvFBC's Direct3D 9 device and the clip signals the BT.601 matrix",
            );
        } else if encodes_on_d3d9 {
            log::info(
                "NvFBC's ARGB grabs are encoded on its Direct3D 9 device (HINDSIGHT_NVFBC_ENCODER=d3d9)",
            );
        }

        let library = Library::load()?;
        let create = prepare(&library, &key, adapter)?;
        let d3d9_device = create_device(&d3d9, adapter)?;
        let slots = [
            create_slot(&d3d9_device, device, size, format)?,
            create_slot(&d3d9_device, device, size, format)?,
            create_slot(&d3d9_device, device, size, format)?,
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
            encoder,
            sync: effective,
            stats,
        };
        capture.session = Some(capture.open_session()?);
        Ok(capture)
    }

    /// Creates the driver's capture object on the device and hands it the surfaces. A refusal
    /// that trying again may change comes back as [`CaptureError::Nvfbc`].
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

    /// Asks the driver for a capture object on the device, presenting the key, and naming the
    /// process whose frames to grab when a switch chose one.
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
        // SAFETY: the parameters are complete; the device, the key and the source outlive the
        // call.
        unsafe { (self.create)(&raw mut params) }.ok()?;
        NonNull::new(params.nvfbc).ok_or(Status::UNSUPPORTED)
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
            // A permanent refusal already reads as unsupported; any other status may pass.
            Err(CaptureError::Nvfbc(..)) => self.reopen.refused(now),
            Err(error) => return Err(error),
        }
        Ok(Frame::Unchanged)
    }

    /// Waits for the GPU to finish the previous tick's grab and copies it into `target`, unless
    /// the encoder reads it in place, and reports which slot it was in, if there was one.
    fn collect(
        &mut self,
        device: &Device,
        target: Option<&ID3D11Texture2D>,
        width: u32,
        height: u32,
    ) -> Result<Option<usize>, CaptureError> {
        let Some(index) = self.pending.take() else {
            return Ok(None);
        };
        let slot = &self.slots[index];
        self.sync.settle(&slot.done, &mut self.stats)?;
        Stats::report_when_due(&mut self.stats, self.sync);
        if !self.handed_over {
            // A grab encoded on the Direct3D 9 device has no target to be copied into, and an
            // NV12 one no Direct3D 11 side to copy from; `start` refused a display the pipeline
            // would not hand the surfaces over for.
            let shared = slot
                .shared
                .as_ref()
                .filter(|_| self.encoder == EncoderOn::Direct3D11)
                .ok_or_else(|| {
                    CaptureError::Unsupported(
                        "NvFBC's grabs encoded on its Direct3D 9 device can only be encoded in place"
                            .into(),
                    )
                })?;
            device.copy_region(super::target_of(target), &shared.texture, width, height);
        }
        Ok(Some(index))
    }

    /// Starts a grab of the display into the next slot.
    fn grab(&mut self) -> Result<Grab, CaptureError> {
        let Some(session) = &self.session else {
            return Ok(Grab::Lost);
        };
        let (status, info) = timed(&mut self.stats, Call::Grab, || {
            session.grab_frame(self.next, self.size, self.switches)
        });
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
                self.follow_grab()?;
                Ok(Grab::Started)
            }
            Status::INVALIDATED_SESSION => Ok(Grab::Lost),
            Status::PROTECTED_CONTENT => Ok(Grab::Withheld),
            status => Err(nvfbc_failed("grab the display with NvFBC")(status)),
        }
    }

    /// Follows the grab just started into the next slot as the sync mode says (usually with
    /// the query that tells when it has finished on the GPU, sent there with it), leaves that
    /// slot for the next tick to collect and moves the following grab on to the slot after it.
    fn follow_grab(&mut self) -> Result<(), CaptureError> {
        let slot = &self.slots[self.next as usize];
        self.sync.follow(&slot.done, &mut self.stats)?;
        self.pending = Some(self.next as usize);
        self.next = (self.next + 1) % SLOT_COUNT;
        Ok(())
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
        device: &Device,
        target: Option<&ID3D11Texture2D>,
        width: u32,
        height: u32,
    ) -> Result<Frame, CaptureError> {
        if is_window_closed(self.window) {
            return Err(CaptureError::Closed);
        }

        let collected = self.collect(device, target, width, height)?;
        match self.grab()? {
            Grab::Started | Grab::Withheld => {}
            Grab::Resized => return Ok(Frame::Resized),
            // The frame just collected is delivered first; the next tick, with nothing left
            // to collect, opens the session again.
            Grab::Lost if collected.is_some() => self.session = None,
            Grab::Lost => {
                self.session = None;
                return self.reopen();
            }
        }
        Ok(match collected {
            Some(index) if self.handed_over => Frame::InSurface(index),
            Some(_) => Frame::Fresh,
            None => Frame::Unchanged,
        })
    }

    fn hand_over_surfaces(&mut self) -> bool {
        self.handed_over = true;
        true
    }

    fn surface(&self, index: usize) -> Option<Surface> {
        let slot = self.slots.get(index)?;
        Some(match &slot.shared {
            Some(shared) if self.encoder == EncoderOn::Direct3D11 => {
                Surface::Texture(shared.texture.clone())
            }
            _ => Surface::Direct3D9 {
                surface: slot.surface.clone(),
                nv12: self.format == PixelFormat::Nv12,
            },
        })
    }

    fn next_surface(&self) -> Option<usize> {
        self.handed_over.then_some(self.next as usize)
    }

    fn encoder_device(&self) -> EncoderDevice {
        if self.encoder == EncoderOn::Direct3D9 {
            EncoderDevice::Direct3D9(self.device.clone())
        } else {
            EncoderDevice::Direct3D
        }
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
        // The driver's object goes before the library that implements it, which is unloaded
        // when the fields are dropped after this.
        self.session = None;
    }
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
        // came from it first: `Capture` releases its session in its own `Drop`.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

/// The entry points of `NvFBC64.dll` that capture calls, valid while the library is loaded.
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
        // SAFETY: looks symbols up in the loaded NvFBC library; each has the documented
        // signature.
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

/// A Direct3D 9 surface NvFBC grabs into, the same memory opened on the encoder's device when
/// it can be, and a query that tells when the grab has finished on the GPU.
struct Slot {
    surface: IDirect3DSurface9,
    /// `None` for an NV12 surface, which only the Direct3D 9 device can see.
    shared: Option<Shared>,
    done: IDirect3DQuery9,
}

/// An ARGB slot's surface as a texture shared with the encoder's Direct3D 11 device.
struct Shared {
    /// The texture the slot's surface is the only level of, kept alive for as long as the
    /// surface.
    _texture: IDirect3DTexture9,
    /// The same memory opened on the encoder's device.
    texture: ID3D11Texture2D,
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
    fn methods(&self) -> &ffi::ToDx9VidVtable {
        // SAFETY: the object is live (see the field), and a live object's table pointer is
        // valid for as long as the library that made it is loaded.
        unsafe { &*(*self.object.as_ptr()).vtable }
    }

    /// Hands the driver the session's surfaces to grab into in `format`, with the cursor drawn
    /// in if `cursor` is set.
    fn set_up(&self, cursor: bool, format: PixelFormat) -> Result<(), Status> {
        let mut setup = ffi::SetupParams::zeroed();
        setup.version = ffi::TODX9VID_SETUP_PARAMS_VER;
        setup.flags = if cursor { ffi::SETUP_WITH_HW_CURSOR } else { 0 };
        setup.mode = format.mode();
        setup.buffer_count = SLOT_COUNT;
        setup.buffers = self.buffers.as_ptr().cast_mut();
        // SAFETY: a live object; the surfaces belong to the device it was created on and live
        // as long as the session.
        unsafe { (self.methods().set_up)(self.object.as_ptr(), &raw mut setup) }.ok()
    }

    /// Starts a grab of the whole display, which is `size` large, into the surface at
    /// `buffer_index` without waiting for the screen to change, unless the switches say
    /// otherwise, and returns the driver's status with its report on the grab.
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
        // Scaling and cropping name their output size; at the display's own size and from its
        // corner they deliver the same picture as the full-size path.
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
        // SAFETY: a live object, set up by `Capture::open_session` before it handed the session
        // out; `info` outlives the call.
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

/// The key's length as the driver's parameter structures hold it.
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
    let entry_points = EntryPoints::look_up(library)?;
    if !is_capture_possible(entry_points.get_status, key, adapter)? {
        turn_on(entry_points.enable)?;
    }
    Ok(entry_points.create)
}

/// Asks the driver whether it will capture `adapter` with this key as things stand.
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
    // SAFETY: the parameters are complete; the key outlives the call.
    unsafe { get_status(&raw mut status) }
        .ok()
        .map_err(nvfbc_failed("read the NvFBC status"))?;
    Ok(status.flags & ffi::STATUS_CAPTURE_POSSIBLE != 0)
}

/// Turns NvFBC on in the driver, which only an administrator may do.
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
    // Software vertex processing, because the device never draws a vertex: it only owns the
    // surfaces NvFBC grabs into. With hardware vertex processing the NVIDIA driver runs a
    // worker thread for the device that costs about 170 Mcycles/s at 240 fps (a third of the
    // backend's CPU); with software vertex processing it starts none, and the grabs, the
    // frames reaching the encoder and the game's frame times are unchanged (measured 2026-10-11,
    // `docs/backend-overhead.md`).
    let flags = D3DCREATE_FPU_PRESERVE
        | D3DCREATE_MULTITHREADED
        | D3DCREATE_SOFTWARE_VERTEXPROCESSING
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

/// A surface the size of the display for grabs in `format`: for ARGB shared with the encoder's
/// device, for NV12 a plain surface only `d3d9` sees.
fn create_slot(
    d3d9: &IDirect3DDevice9Ex,
    device: &Device,
    size: (u32, u32),
    format: PixelFormat,
) -> Result<Slot, CaptureError> {
    let (surface, shared) = match format {
        PixelFormat::Argb => {
            let (texture, handle) = create_shared_texture(d3d9, size)?;
            // SAFETY: the texture has one level.
            let surface =
                unsafe { texture.GetSurfaceLevel(0) }.map_err(failed("open the NvFBC surface"))?;
            let shared = Shared {
                _texture: texture,
                texture: open_shared(device, handle)?,
            };
            (surface, Some(shared))
        }
        PixelFormat::Nv12 => (create_nv12_surface(d3d9, size)?, None),
    };
    // SAFETY: creates a query on a live device.
    let done = unsafe { d3d9.CreateQuery(D3DQUERYTYPE_EVENT) }
        .map_err(failed("create the NvFBC completion query"))?;
    Ok(Slot {
        surface,
        shared,
        done,
    })
}

/// An NV12 plain surface of `width` × `height` that the driver can grab into. Direct3D 9 makes
/// NV12 neither as a texture nor as a render target, and does not share a plain surface with
/// Direct3D 11, so the surface is not shared.
fn create_nv12_surface(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<IDirect3DSurface9, CaptureError> {
    let mut surface = None;
    // SAFETY: an unshared plain surface in the default pool, so the handle pointer may be
    // null; the out-parameter is written on success.
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

/// A render-target texture of `width` × `height` that the driver can grab into, and the
/// handle that shares it with other devices.
fn create_shared_texture(
    d3d9: &IDirect3DDevice9Ex,
    (width, height): (u32, u32),
) -> Result<(IDirect3DTexture9, HANDLE), CaptureError> {
    let mut texture = None;
    let mut handle = HANDLE::default();
    // SAFETY: a shared texture must be in the default pool, as it is; both out-parameters are
    // written on success.
    let texture = unsafe {
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
    Ok((texture, handle))
}

/// The texture behind a handle a Direct3D 9Ex device on the encoding GPU shared, opened on the
/// encoder's device.
fn open_shared(device: &Device, handle: HANDLE) -> Result<ID3D11Texture2D, CaptureError> {
    let mut shared: Option<ID3D11Texture2D> = None;
    // SAFETY: a shared handle from a Direct3D 9Ex device on the same adapter; the
    // out-parameter is written on success.
    unsafe { device.d3d.OpenSharedResource(handle, &raw mut shared) }
        .and_then(|()| shared.ok_or_else(windows::core::Error::empty))
        .map_err(failed("share the NvFBC surface with the encoder"))
}

/// The surfaces of `slots` as the driver is handed them at setup.
fn output_buffers(slots: &[Slot; SLOTS]) -> [ffi::OutputBuffer; SLOTS] {
    std::array::from_fn(|index| {
        let mut buffer = ffi::OutputBuffer::zeroed();
        buffer.primary = slots[index].surface.as_raw();
        buffer
    })
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

/// Refuses a display with an odd width or height for grabs encoded on the Direct3D 9 device:
/// NVENC encodes even sizes, and such a grab can only be read in place, uncropped.
fn require_even((width, height): (u32, u32)) -> Result<(), CaptureError> {
    if width % 2 == 0 && height % 2 == 0 {
        return Ok(());
    }
    Err(CaptureError::Unsupported(format!(
        "the display is {width}×{height}; NvFBC's grabs encoded on its Direct3D 9 device are read in place and need an even width and height, so unset HINDSIGHT_NVFBC_FORMAT and HINDSIGHT_NVFBC_ENCODER"
    )))
}

/// Wraps a driver status with the step that failed, for `map_err`; a refusal that trying again
/// would not change reads as unsupported.
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
    fn only_d3d9_moves_the_encoder() {
        assert_eq!(EncoderOn::parse(None), EncoderOn::Direct3D11);
        assert_eq!(EncoderOn::parse(Some("d3d11")), EncoderOn::Direct3D11);
        assert_eq!(EncoderOn::parse(Some(" D3D9 ")), EncoderOn::Direct3D9);
    }

    #[test]
    fn nv12_names_its_fourcc() {
        assert_eq!(D3DFMT_NV12.0, 0x3231_564E);
    }

    #[test]
    fn odd_displays_are_refused_for_nv12() {
        assert!(require_even((2560, 1440)).is_ok());
        assert!(matches!(
            require_even((1366, 767)),
            Err(CaptureError::Unsupported(_))
        ));
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
}
