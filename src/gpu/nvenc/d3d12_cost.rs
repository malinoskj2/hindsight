//! By-hand measurement of what each NVENC call costs the calling thread per frame on Direct3D
//! 11, 12 and 9Ex sessions: `cargo test --release d3d12_encode_cost -- --ignored --nocapture`.

use super::{
    Session, SessionDevice, Settings, configure, create_input_texture,
    ffi::{self, Status},
    initialize_params,
};
use crate::{
    config::Codec,
    gpu::device::{Device, null_output},
    replay::Matrix,
};
use bytemuck::Zeroable;
use std::{
    ffi::c_void,
    ptr,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LUID, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::D3D_FEATURE_LEVEL_11_0,
            Direct3D9::{
                D3D_SDK_VERSION, D3DCREATE_FPU_PRESERVE, D3DCREATE_MULTITHREADED,
                D3DCREATE_NOWINDOWCHANGES, D3DCREATE_SOFTWARE_VERTEXPROCESSING, D3DDEVTYPE_HAL,
                D3DFMT_A8R8G8B8, D3DFMT_UNKNOWN, D3DPOOL_DEFAULT, D3DPRESENT_INTERVAL_IMMEDIATE,
                D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD, D3DUSAGE_RENDERTARGET,
                Direct3DCreate9Ex, IDirect3DDevice9Ex, IDirect3DSurface9, IDirect3DTexture9,
            },
            Direct3D11::ID3D11Texture2D,
            Direct3D12::{
                D3D12_FENCE_FLAG_NONE, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
                D3D12_HEAP_TYPE, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
                D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATES,
                D3D12_TEXTURE_LAYOUT_ROW_MAJOR, D3D12CreateDevice, ID3D12Device, ID3D12Fence,
                ID3D12Resource,
            },
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
        },
        System::{
            Threading::{CreateEventW, WaitForSingleObject},
            WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime},
        },
    },
    core::{Interface, Owned},
};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FRAME_RATE: u32 = 240;
const FRAMES: usize = 2400;
const WARMUP: usize = 60;
const SLOTS: usize = 3;
const TIMEOUT_MS: u32 = 2000;

const CALLS: [&str; 6] = ["map", "encode", "wait", "lock+copy", "unlock", "unmap"];
const MAP: usize = 0;
const ENCODE: usize = 1;
const WAIT: usize = 2;
const LOCK: usize = 3;
const UNLOCK: usize = 4;
const UNMAP: usize = 5;

type Costs = [u64; CALLS.len()];

/// `NV_ENC_FENCE_POINT_D3D12` (`nvEncodeAPI.h`, SDK 12.1, nv-codec-headers n12.1.14.0).
#[repr(C)]
#[derive(Zeroable)]
struct FencePoint {
    version: u32,
    _reserved: u32,
    fence: *mut c_void,
    wait_value: u64,
    signal_value: u64,
    /// `bWait` in bit 0, `bSignal` in bit 1.
    flags: u32,
    _reserved1: [u32; 7],
}

const FENCE_WAIT: u32 = 1 << 0;
const FENCE_SIGNAL: u32 = 1 << 1;

/// `NV_ENC_INPUT_RESOURCE_D3D12` and `NV_ENC_OUTPUT_RESOURCE_D3D12`, which share one layout.
#[repr(C)]
#[derive(Zeroable)]
struct ResourceD3D12 {
    version: u32,
    _reserved: u32,
    mapped: ffi::Mapped,
    fence_point: FencePoint,
    _reserved1: [u32; 16],
    _reserved2: [*mut c_void; 16],
}

const _: () = assert!(size_of::<FencePoint>() == 64);
const _: () = assert!(size_of::<ResourceD3D12>() == 272);
const _: () = assert!(std::mem::offset_of!(ResourceD3D12, fence_point) == 16);

const D3D12_STRUCT_VER: u32 = ffi::struct_version(1);

#[derive(Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches of one measurement"
)]
struct Case {
    name: &'static str,
    d3d12: bool,
    d3d9: bool,
    lock_no_wait: bool,
    asynchronous: bool,
    persistent_map: bool,
    input_fence: bool,
}

const CASES: [Case; 10] = [
    Case {
        name: "d3d11 async, map per frame, lock without waiting",
        d3d12: false,
        d3d9: false,
        lock_no_wait: true,
        asynchronous: true,
        persistent_map: false,
        input_fence: true,
    },
    Case {
        name: "d3d9 async, map per frame (nvfbc encoder d3d9)",
        d3d12: false,
        d3d9: true,
        lock_no_wait: false,
        asynchronous: true,
        persistent_map: false,
        input_fence: true,
    },
    Case {
        name: "d3d9 async, map per frame, lock without waiting",
        d3d12: false,
        d3d9: true,
        lock_no_wait: true,
        asynchronous: true,
        persistent_map: false,
        input_fence: true,
    },
    Case {
        name: "d3d11 async, map per frame (hindsight today)",
        d3d12: false,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: true,
        persistent_map: false,
        input_fence: true,
    },
    Case {
        name: "d3d11 async, mapped once",
        d3d12: false,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: true,
        persistent_map: true,
        input_fence: true,
    },
    Case {
        name: "d3d11 sync, mapped once",
        d3d12: false,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: false,
        persistent_map: true,
        input_fence: true,
    },
    Case {
        name: "d3d12 sync, map per frame (as NvEncoderD3D12)",
        d3d12: true,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: false,
        persistent_map: false,
        input_fence: true,
    },
    Case {
        name: "d3d12 sync, mapped once",
        d3d12: true,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: false,
        persistent_map: true,
        input_fence: true,
    },
    Case {
        name: "d3d12 async, mapped once",
        d3d12: true,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: true,
        persistent_map: true,
        input_fence: true,
    },
    Case {
        name: "d3d12 async, mapped once, no input fence",
        d3d12: true,
        d3d9: false,
        lock_no_wait: false,
        asynchronous: true,
        persistent_map: true,
        input_fence: false,
    },
];

#[test]
#[ignore = "encodes on the GPU for about a minute; run by hand"]
fn d3d12_encode_cost() {
    let mut results = Vec::new();
    let only = std::env::var("HINDSIGHT_COST_CASES").unwrap_or_default();
    for case in CASES
        .into_iter()
        .filter(|case| case.name.contains(only.as_str()))
    {
        println!("\n== {}", case.name);
        match Bench::open(case) {
            Ok(mut bench) => {
                let report = bench.run();
                bench.teardown();
                report.print();
                results.push((case, report));
            }
            Err(error) => println!("could not open: {error}"),
        }
    }
    println!("\n== encode path, mean thread cycles per frame (sum of every NVENC call)");
    let baseline = results.first().map(|(_, report)| report.path_mean());
    for (case, report) in &results {
        let mean = report.path_mean();
        let change = baseline.map_or(String::new(), |baseline| {
            format!(
                " ({:+.0}% against the first case)",
                (mean as f64 / baseline as f64 - 1.0) * 100.0
            )
        });
        println!("{:<48} {mean:>9}{change}", case.name);
    }
}

fn thread_cycles() -> u64 {
    let mut cycles = 0;
    let thread = HANDLE(winsafe::HTHREAD::GetCurrentThread().ptr());
    // SAFETY: the calling thread's pseudo-handle and one integer to write.
    unsafe { QueryThreadCycleTime(thread, &raw mut cycles) }.expect("query the thread's cycles");
    cycles
}

fn process_cycles() -> u64 {
    let mut cycles = 0;
    let process = HANDLE(winsafe::HPROCESS::GetCurrentProcess().ptr());
    // SAFETY: this process's pseudo-handle and one integer to write.
    unsafe { QueryProcessCycleTime(process, &raw mut cycles) }.expect("query the process's cycles");
    cycles
}

fn timed<T>(costs: &mut Costs, call: usize, body: impl FnOnce() -> T) -> T {
    let start = thread_cycles();
    let result = body();
    costs[call] += thread_cycles() - start;
    result
}

/// What the bench asks of a session that the encoder does not.
trait BenchSession {
    fn assert_ok(&self, call: &'static str, status: Status);
    fn initialize(&self, settings: Settings, asynchronous: bool) -> Result<(), String>;
    fn register(
        &self,
        resource: *mut c_void,
        size: (u32, u32),
        format: u32,
        usage: u32,
    ) -> ffi::Registered;
    fn map(&self, registered: ffi::Registered) -> (ffi::Mapped, u32);
    fn unmap(&self, mapped: ffi::Mapped);
    fn with_pointer(&self, call: &'static str, entry: ffi::WithPointer, pointer: *mut c_void);
}

impl BenchSession for Session {
    fn assert_ok(&self, call: &'static str, status: Status) {
        if let Err(error) = self.check(call, status) {
            panic!("{error}");
        }
    }

    fn initialize(&self, settings: Settings, asynchronous: bool) -> Result<(), String> {
        let mut preset = ffi::PresetConfig::zeroed();
        preset.version = ffi::PRESET_CONFIG_VER;
        preset.config.version = ffi::CONFIG_VER;
        let get_preset = self.api.get_encode_preset_config_ex;
        // SAFETY: a versioned, writable preset structure.
        let status = unsafe {
            get_preset(
                self.handle,
                ffi::CODEC_AV1,
                ffi::PRESET_P4,
                ffi::TUNING_HIGH_QUALITY,
                &raw mut preset,
            )
        };
        self.assert_ok("nvEncGetEncodePresetConfigEx", status);
        let mut config = preset.config;
        configure(&mut config, settings);
        let mut params = initialize_params(settings);
        params.encode_config = &raw mut config;
        params.enable_encode_async = u32::from(asynchronous);
        // A Direct3D 12 session must be told the input format up front; others ignore it.
        params.buffer_format = ffi::BUFFER_FORMAT_ARGB;
        let initialize = self.api.initialize_encoder;
        // SAFETY: `params` and the configuration it points to live across the call.
        let status = unsafe { initialize(self.handle, &raw mut params) };
        self.check("nvEncInitializeEncoder", status)
            .map_err(|error| error.to_string())
    }

    fn register(
        &self,
        resource: *mut c_void,
        (width, height): (u32, u32),
        format: u32,
        usage: u32,
    ) -> ffi::Registered {
        let mut params = ffi::RegisterResource::zeroed();
        params.version = ffi::REGISTER_RESOURCE_VER;
        params.resource_type = ffi::INPUT_RESOURCE_TYPE_DIRECTX;
        params.width = width;
        params.height = height;
        params.resource_to_register = resource;
        params.buffer_format = format;
        params.buffer_usage = usage;
        let register = self.api.register_resource;
        // SAFETY: the resource outlives its registration, which `teardown` undoes.
        let status = unsafe { register(self.handle, &raw mut params) };
        self.assert_ok("nvEncRegisterResource", status);
        params.registered_resource
    }

    fn map(&self, registered: ffi::Registered) -> (ffi::Mapped, u32) {
        let mut map = ffi::MapInputResource::zeroed();
        map.version = ffi::MAP_INPUT_RESOURCE_VER;
        map.registered_resource = registered;
        let map_input = self.api.map_input_resource;
        // SAFETY: a registered resource that is not mapped.
        let status = unsafe { map_input(self.handle, &raw mut map) };
        self.assert_ok("nvEncMapInputResource", status);
        (map.mapped_resource, map.mapped_buffer_format)
    }

    fn unmap(&self, mapped: ffi::Mapped) {
        let unmap = self.api.unmap_input_resource;
        // SAFETY: a mapping that has not been undone, of a frame that has completed.
        let status = unsafe { unmap(self.handle, mapped) };
        self.assert_ok("nvEncUnmapInputResource", status);
    }

    fn with_pointer(&self, call: &'static str, entry: ffi::WithPointer, pointer: *mut c_void) {
        // SAFETY: `pointer` is the kind of handle the entry takes, still live.
        let status = unsafe { entry(self.handle, pointer) };
        self.assert_ok(call, status);
    }
}

enum Backend {
    D3d11 {
        _device: Device,
        texture: ID3D11Texture2D,
    },
    D3d9 {
        // Dropped before the device.
        surface: IDirect3DSurface9,
        _texture: IDirect3DTexture9,
        _device: IDirect3DDevice9Ex,
    },
    D3d12 {
        device: ID3D12Device,
        texture: ID3D12Resource,
        /// Signalled to 1 once; NVENC waits on it before every frame.
        input_fence: ID3D12Fence,
        /// NVENC signals it to the frame's number + 1 once the frame is written.
        output_fence: ID3D12Fence,
        fence_event: Owned<HANDLE>,
    },
}

enum Output {
    Bitstream(ffi::Bitstream),
    Readback {
        _buffer: ID3D12Resource,
        registered: ffi::Registered,
        mapped: Option<ffi::Mapped>,
        /// Boxed: NVENC holds its address while the frame is in flight.
        resource: Box<ResourceD3D12>,
    },
}

struct Slot {
    output: Output,
    event: Owned<HANDLE>,
    in_flight: Option<InFlight>,
}

struct InFlight {
    input: Option<ffi::Mapped>,
    output: Option<ffi::Mapped>,
    fence_value: u64,
    _input_resource: Option<Box<ResourceD3D12>>,
}

struct Report {
    frames: Vec<Costs>,
    process_cycles: u64,
    loop_thread_cycles: u64,
    bytes: usize,
    elapsed: Duration,
}

impl Report {
    fn path_mean(&self) -> u64 {
        mean(
            &self
                .frames
                .iter()
                .map(|costs| costs.iter().sum())
                .collect::<Vec<u64>>(),
        )
    }

    fn print(&self) {
        let count = self.frames.len() as u64;
        println!(
            "{count} frames measured in {:.2} s, {} bytes per frame",
            self.elapsed.as_secs_f64(),
            self.bytes / self.frames.len().max(1)
        );
        println!("{:<12} {:>10} {:>10}", "call", "mean", "median");
        for (index, name) in CALLS.iter().enumerate() {
            let column: Vec<u64> = self.frames.iter().map(|costs| costs[index]).collect();
            println!(
                "{name:<12} {:>10} {:>10}",
                mean(&column),
                median(column.clone())
            );
        }
        let path: Vec<u64> = self.frames.iter().map(|costs| costs.iter().sum()).collect();
        println!(
            "{:<12} {:>10} {:>10}",
            "encode path",
            mean(&path),
            median(path.clone())
        );
        println!(
            "thread cycles per frame, whole loop: {}",
            self.loop_thread_cycles / count.max(1)
        );
        println!(
            "process cycles per frame, every thread: {}",
            self.process_cycles / count.max(1)
        );
    }
}

fn mean(values: &[u64]) -> u64 {
    values.iter().sum::<u64>() / (values.len() as u64).max(1)
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

struct Bench {
    // Dropped before the resources it was given.
    session: Session,
    case: Case,
    registered_input: ffi::Registered,
    input_mapping: Option<(ffi::Mapped, u32)>,
    slots: Vec<Slot>,
    backend: Backend,
    copy: Vec<u8>,
}

impl Bench {
    fn open(case: Case) -> Result<Self, String> {
        let settings = Settings {
            codec: Codec::Av1,
            width: WIDTH,
            height: HEIGHT,
            frame_rate: FRAME_RATE,
            bitrate_bps: 100_000_000,
            matrix: Matrix::Bt709,
        };
        let device = Device::create().map_err(|error| error.to_string())?;
        let (session, backend) = if case.d3d9 {
            let backend = open_d3d9(&device).map_err(|error| error.to_string())?;
            let Backend::D3d9 { _device: d3d9, .. } = &backend else {
                unreachable!("open_d3d9 opens Direct3D 9")
            };
            (
                Session::open(SessionDevice::Direct3D9(d3d9.clone()))
                    .map_err(|error| error.to_string())?,
                backend,
            )
        } else if case.d3d12 {
            let backend = open_d3d12(&device).map_err(|error| error.to_string())?;
            let Backend::D3d12 { device: d3d12, .. } = &backend else {
                unreachable!("open_d3d12 opens Direct3D 12")
            };
            (
                Session::open(SessionDevice::Direct3D12(d3d12.clone()))
                    .map_err(|error| error.to_string())?,
                backend,
            )
        } else {
            let texture =
                create_input_texture(&device.d3d, settings).map_err(|error| error.to_string())?;
            let session = Session::open(SessionDevice::Direct3D11(device.d3d.clone()))
                .map_err(|error| error.to_string())?;
            let backend = Backend::D3d11 {
                _device: device,
                texture,
            };
            (session, backend)
        };
        BenchSession::initialize(&session, settings, case.asynchronous)?;
        let mut bench = Self {
            session,
            case,
            registered_input: ptr::null_mut(),
            input_mapping: None,
            slots: Vec::with_capacity(SLOTS),
            backend,
            copy: Vec::new(),
        };
        bench.prepare();
        Ok(bench)
    }

    fn prepare(&mut self) {
        let texture = match &self.backend {
            Backend::D3d11 { texture, .. } => texture.as_raw(),
            Backend::D3d9 { surface, .. } => surface.as_raw(),
            Backend::D3d12 { texture, .. } => texture.as_raw(),
        };
        self.registered_input = self.session.register(
            texture,
            (WIDTH, HEIGHT),
            ffi::BUFFER_FORMAT_ARGB,
            ffi::INPUT_IMAGE,
        );
        if self.case.persistent_map {
            self.input_mapping = Some(self.session.map(self.registered_input));
        }
        for _ in 0..SLOTS {
            let slot = self.create_slot();
            self.slots.push(slot);
        }
    }

    fn create_slot(&self) -> Slot {
        // SAFETY: an unnamed auto-reset event with default security.
        let event = unsafe { CreateEventW(None, false, false, None) }.expect("create an event");
        // SAFETY: just created; only this `Owned` closes it.
        let event = unsafe { Owned::new(event) };
        if self.case.asynchronous {
            let mut params = ffi::EventParams::zeroed();
            params.version = ffi::EVENT_PARAMS_VER;
            params.completion_event = event.0;
            let register = self.session.api.register_async_event;
            // SAFETY: the event stays open until the session is destroyed.
            let status = unsafe { register(self.session.handle, &raw mut params) };
            self.session.assert_ok("nvEncRegisterAsyncEvent", status);
        }
        let output = match &self.backend {
            Backend::D3d11 { .. } | Backend::D3d9 { .. } => {
                let mut buffer = ffi::CreateBitstreamBuffer::zeroed();
                buffer.version = ffi::CREATE_BITSTREAM_BUFFER_VER;
                let create = self.session.api.create_bitstream_buffer;
                // SAFETY: a versioned structure for an initialized session.
                let status = unsafe { create(self.session.handle, &raw mut buffer) };
                self.session.assert_ok("nvEncCreateBitstreamBuffer", status);
                Output::Bitstream(buffer.bitstream_buffer)
            }
            Backend::D3d12 { device, .. } => self.create_readback(device),
        };
        Slot {
            output,
            event,
            in_flight: None,
        }
    }

    /// Twice the picture's bytes, as `NvEncoderD3D12.cpp` sizes them.
    fn create_readback(&self, device: &ID3D12Device) -> Output {
        let bytes = 2 * WIDTH * HEIGHT * 4;
        let buffer = create_resource(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_description(u64::from(bytes)),
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
        .expect("create a readback buffer");
        let registered = self.session.register(
            buffer.as_raw(),
            (bytes, 1),
            ffi::BUFFER_FORMAT_U8,
            ffi::OUTPUT_BITSTREAM,
        );
        let mapped = self
            .case
            .persistent_map
            .then(|| self.session.map(registered).0);
        let mut resource = Box::new(ResourceD3D12::zeroed());
        resource.version = D3D12_STRUCT_VER;
        resource.fence_point.version = D3D12_STRUCT_VER;
        Output::Readback {
            _buffer: buffer,
            registered,
            mapped,
            resource,
        }
    }

    fn run(&mut self) -> Report {
        let period = Duration::from_secs(1) / FRAME_RATE;
        let mut frames = Vec::with_capacity(FRAMES - WARMUP);
        let mut bytes = 0;
        let start = Instant::now();
        let (mut process_start, mut thread_start, mut measure_start) = (0, 0, start);
        for index in 0..FRAMES {
            if index == WARMUP {
                (process_start, thread_start, measure_start) =
                    (process_cycles(), thread_cycles(), Instant::now());
            }
            let deadline = start + period * u32::try_from(index).expect("few frames");
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let mut costs = Costs::default();
            let slot = index % SLOTS;
            if self.slots[slot].in_flight.is_some() {
                bytes += self.collect(slot, &mut costs);
            }
            self.submit(slot, index as u64, &mut costs);
            if index >= WARMUP {
                frames.push(costs);
            }
        }
        let report = Report {
            process_cycles: process_cycles() - process_start,
            loop_thread_cycles: thread_cycles() - thread_start,
            elapsed: measure_start.elapsed(),
            frames,
            bytes,
        };
        for slot in 0..SLOTS {
            if self.slots[slot].in_flight.is_some() {
                self.collect(slot, &mut Costs::default());
            }
        }
        report
    }

    fn submit(&mut self, slot: usize, index: u64, costs: &mut Costs) {
        let (input, format, own_input) = if let Some((mapped, format)) = self.input_mapping {
            (mapped, format, false)
        } else {
            let (mapped, format) = timed(costs, MAP, || self.session.map(self.registered_input));
            (mapped, format, true)
        };
        let fence_value = index + 1;
        let mut picture = ffi::PicParams::zeroed();
        picture.version = ffi::PIC_PARAMS_VER;
        picture.input_width = WIDTH;
        picture.input_height = HEIGHT;
        picture.input_time_stamp = index;
        picture.buffer_format = format;
        picture.picture_struct = ffi::PIC_STRUCT_FRAME;
        if self.case.asynchronous {
            picture.completion_event = self.slots[slot].event.0;
        }
        let mut own_output = None;
        let mut input_resource = None;
        match &self.backend {
            Backend::D3d11 { .. } | Backend::D3d9 { .. } => {
                picture.input_buffer = input;
                let Output::Bitstream(bitstream) = self.slots[slot].output else {
                    unreachable!("a Direct3D 11 session has bitstream buffers")
                };
                picture.output_bitstream = bitstream;
            }
            Backend::D3d12 {
                input_fence,
                output_fence,
                ..
            } => {
                let mut resource = Box::new(ResourceD3D12::zeroed());
                resource.version = D3D12_STRUCT_VER;
                resource.mapped = input;
                if self.case.input_fence {
                    resource.fence_point = fence_point(input_fence, FENCE_WAIT, 1, 0);
                } else {
                    resource.fence_point.version = D3D12_STRUCT_VER;
                }
                picture.input_buffer = (&raw mut *resource).cast();
                input_resource = Some(resource);
                let session = &self.session;
                let Output::Readback {
                    registered,
                    mapped,
                    resource: output,
                    ..
                } = &mut self.slots[slot].output
                else {
                    unreachable!("a Direct3D 12 session has readback buffers")
                };
                let mapped = mapped.unwrap_or_else(|| {
                    let mapped = timed(costs, MAP, || session.map(*registered).0);
                    own_output = Some(mapped);
                    mapped
                });
                output.mapped = mapped;
                output.fence_point = fence_point(output_fence, FENCE_SIGNAL, 0, fence_value);
                picture.output_bitstream = (&raw mut **output).cast();
            }
        }
        let encode = self.session.api.encode_picture;
        // SAFETY: input mapped, output idle, and every pointed-to structure lives until collected.
        let status = timed(costs, ENCODE, || unsafe {
            encode(self.session.handle, &raw mut picture)
        });
        self.session.assert_ok("nvEncEncodePicture", status);
        self.slots[slot].in_flight = Some(InFlight {
            input: own_input.then_some(input),
            output: own_output,
            fence_value,
            _input_resource: input_resource,
        });
    }

    fn collect(&mut self, slot: usize, costs: &mut Costs) -> usize {
        let frame = self.slots[slot]
            .in_flight
            .take()
            .expect("a frame in flight");
        if self.case.asynchronous {
            let event = *self.slots[slot].event;
            // SAFETY: the slot's open event.
            let waited = timed(costs, WAIT, || unsafe {
                WaitForSingleObject(event, TIMEOUT_MS)
            });
            assert_eq!(
                waited, WAIT_OBJECT_0,
                "NVENC did not finish a frame in time"
            );
        } else if let Backend::D3d12 {
            output_fence,
            fence_event,
            ..
        } = &self.backend
        {
            timed(costs, WAIT, || {
                await_fence(output_fence, frame.fence_value, **fence_event);
            });
        }
        let output = match &mut self.slots[slot].output {
            Output::Bitstream(bitstream) => *bitstream,
            Output::Readback { resource, .. } => (&raw mut **resource).cast(),
        };
        let mut lock = ffi::LockBitstream::zeroed();
        lock.version = ffi::LOCK_BITSTREAM_VER;
        lock.output_bitstream = output;
        lock.flags = if self.case.lock_no_wait {
            ffi::LOCK_DO_NOT_WAIT
        } else {
            0
        };
        let lock_bitstream = self.session.api.lock_bitstream;
        let copy = &mut self.copy;
        let session = &self.session;
        let bytes = timed(costs, LOCK, || {
            // SAFETY: the frame is done or the lock waits for it; unlocked right after the copy.
            let status = unsafe { lock_bitstream(session.handle, &raw mut lock) };
            session.assert_ok("nvEncLockBitstream", status);
            copy.clear();
            if !lock.bitstream_buffer_ptr.is_null() {
                // SAFETY: while locked, `bitstream_size_in_bytes` bytes are readable there.
                copy.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(
                        lock.bitstream_buffer_ptr.cast::<u8>(),
                        lock.bitstream_size_in_bytes as usize,
                    )
                });
            }
            copy.len()
        });
        timed(costs, UNLOCK, || {
            session.with_pointer("nvEncUnlockBitstream", session.api.unlock_bitstream, output);
        });
        for mapped in [frame.output, frame.input].into_iter().flatten() {
            timed(costs, UNMAP, || session.unmap(mapped));
        }
        bytes
    }

    fn teardown(&mut self) {
        let session = &self.session;
        if let Some((mapped, _)) = self.input_mapping.take() {
            session.unmap(mapped);
        }
        session.with_pointer(
            "nvEncUnregisterResource",
            session.api.unregister_resource,
            self.registered_input,
        );
        for slot in &mut self.slots {
            match &mut slot.output {
                Output::Bitstream(bitstream) => session.with_pointer(
                    "nvEncDestroyBitstreamBuffer",
                    session.api.destroy_bitstream_buffer,
                    *bitstream,
                ),
                Output::Readback {
                    registered, mapped, ..
                } => {
                    if let Some(mapped) = mapped.take() {
                        session.unmap(mapped);
                    }
                    session.with_pointer(
                        "nvEncUnregisterResource",
                        session.api.unregister_resource,
                        *registered,
                    );
                }
            }
        }
    }
}

fn open_d3d9(device: &Device) -> windows::core::Result<Backend> {
    // SAFETY: creates the Direct3D 9Ex object; no other preconditions.
    let d3d9 = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION)? };
    let luid = device.luid()?;
    // SAFETY: counts the adapters of a live object.
    let adapter = (0..unsafe { d3d9.GetAdapterCount() })
        .find(|&adapter| {
            let mut found = LUID::default();
            // SAFETY: every index below the count names an adapter; written on success.
            unsafe { d3d9.GetAdapterLUID(adapter, &raw mut found) }.is_ok() && found == luid
        })
        .ok_or_else(null_output)?;
    let mut parameters = D3DPRESENT_PARAMETERS {
        BackBufferWidth: 1,
        BackBufferHeight: 1,
        BackBufferFormat: D3DFMT_UNKNOWN,
        BackBufferCount: 1,
        SwapEffect: D3DSWAPEFFECT_DISCARD,
        hDeviceWindow: HWND(winsafe::HWND::GetDesktopWindow().ptr()),
        Windowed: true.into(),
        PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE.cast_unsigned(),
        ..Default::default()
    };
    let flags = D3DCREATE_FPU_PRESERVE
        | D3DCREATE_MULTITHREADED
        | D3DCREATE_SOFTWARE_VERTEXPROCESSING
        | D3DCREATE_NOWINDOWCHANGES;
    let mut created = None;
    // SAFETY: windowed parameters on the desktop window; written on success.
    unsafe {
        d3d9.CreateDeviceEx(
            adapter,
            D3DDEVTYPE_HAL,
            HWND::default(),
            flags.cast_unsigned(),
            &raw mut parameters,
            ptr::null_mut(),
            &raw mut created,
        )?;
    }
    let d3d9_device = created.ok_or_else(null_output)?;
    let mut texture = None;
    // SAFETY: a default-pool render target; written on success; no shared handle asked for.
    unsafe {
        d3d9_device.CreateTexture(
            WIDTH,
            HEIGHT,
            1,
            D3DUSAGE_RENDERTARGET.cast_unsigned(),
            D3DFMT_A8R8G8B8,
            D3DPOOL_DEFAULT,
            &raw mut texture,
            ptr::null_mut(),
        )?;
    }
    let texture: IDirect3DTexture9 = texture.ok_or_else(null_output)?;
    // SAFETY: the texture has one level.
    let surface = unsafe { texture.GetSurfaceLevel(0)? };
    Ok(Backend::D3d9 {
        surface,
        _texture: texture,
        _device: d3d9_device,
    })
}

fn fence_point(fence: &ID3D12Fence, flags: u32, wait_value: u64, signal_value: u64) -> FencePoint {
    FencePoint {
        version: D3D12_STRUCT_VER,
        _reserved: 0,
        fence: fence.as_raw(),
        wait_value,
        signal_value,
        flags,
        _reserved1: [0; 7],
    }
}

fn await_fence(fence: &ID3D12Fence, value: u64, event: HANDLE) {
    // SAFETY: reads a live fence.
    if unsafe { fence.GetCompletedValue() } >= value {
        return;
    }
    // SAFETY: a live fence and an open event.
    unsafe { fence.SetEventOnCompletion(value, event) }.expect("set a fence event");
    // SAFETY: an open event.
    let waited = unsafe { WaitForSingleObject(event, TIMEOUT_MS) };
    assert_eq!(
        waited, WAIT_OBJECT_0,
        "NVENC did not signal the output fence in time"
    );
}

fn open_d3d12(device: &Device) -> windows::core::Result<Backend> {
    // SAFETY: reads the adapter of a live device.
    let adapter = unsafe { device.dxgi()?.GetAdapter()? };
    let mut d3d12: Option<ID3D12Device> = None;
    // SAFETY: a live adapter and an out-parameter written on success.
    unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &raw mut d3d12)? };
    let d3d12 = d3d12.ok_or_else(null_output)?;
    let texture = create_resource(
        &d3d12,
        D3D12_HEAP_TYPE_DEFAULT,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(WIDTH),
            Height: HEIGHT,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        },
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    // SAFETY: plain fence creation on a live device.
    let input_fence: ID3D12Fence = unsafe { d3d12.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
    // SAFETY: as above.
    let output_fence: ID3D12Fence = unsafe { d3d12.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
    // SAFETY: a live fence.
    unsafe { input_fence.Signal(1)? };
    // SAFETY: an unnamed auto-reset event with default security.
    let fence_event = unsafe { CreateEventW(None, false, false, None)? };
    Ok(Backend::D3d12 {
        device: d3d12,
        texture,
        input_fence,
        output_fence,
        // SAFETY: just created; only this `Owned` closes it.
        fence_event: unsafe { Owned::new(fence_event) },
    })
}

fn buffer_description(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    }
}

fn create_resource(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    description: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let properties = D3D12_HEAP_PROPERTIES {
        Type: heap,
        ..Default::default()
    };
    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: the descriptions live across the call; the out-parameter is written on success.
    unsafe {
        device.CreateCommittedResource(
            &raw const properties,
            D3D12_HEAP_FLAG_NONE,
            description,
            state,
            None,
            &raw mut resource,
        )?;
    }
    resource.ok_or_else(null_output)
}
