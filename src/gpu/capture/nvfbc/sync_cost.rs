//! A by-hand measurement, not part of the program: what each way of following an NvFBC grab
//! (see [`super::sync`]) costs the capturing thread per frame, grabbing the primary monitor at
//! 240 frames a second with the saved key, nothing encoding.
//!
//! Run it on a machine with an NVIDIA GPU, with nothing else measuring:
//!
//! ```text
//! cargo test --release grab_sync_cost -- --ignored --nocapture
//! ```
//!
//! Each case opens its own capture, grabs for a warm-up second and then for eight seconds,
//! handing every grab over as the in-place path does, and prints the thread cycles per frame of
//! the grab call, the query's issue, its flushing look and the handover's wait, with the
//! process's cycles per frame. A last probe follows grabs with the query but no flush and
//! looks at it without flushing, to tell whether anything but the flush sends the grab to the
//! GPU. With no game drawing, the GPU reaches every grab at once, so the late handovers a busy
//! GPU causes (and that `sleep` addresses) only show in a live run with
//! `HINDSIGHT_GRAB_SYNC_STATS=1`.

use super::{Capture, EncoderOn, PixelFormat, Setup, Switches, sync::GrabSync, sync::Stats};
use crate::{gpu::device::Device, service::Target};
use std::{
    ptr,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::S_OK,
        Graphics::Direct3D9::D3DISSUE_END,
        System::{Threading::GetCurrentProcess, WindowsProgramming::QueryProcessCycleTime},
    },
    core::Interface,
};

const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 240);
/// Frames grabbed before the counts start.
const WARMUP: u32 = 240;
/// Frames counted, under the ten seconds after which the capture would log and reset them.
const FRAMES: u32 = 1920;
/// Grabs the probe follows without flushing.
const PROBE_FRAMES: u32 = 240;
/// How long the probe waits for an unflushed grab to land before calling it unsent.
const PROBE_PATIENCE: Duration = Duration::from_millis(4);

/// The cases, each a pixel format, the device NVENC would read on, and the mode asked for.
const CASES: [(PixelFormat, EncoderOn, GrabSync); 5] = [
    (PixelFormat::Argb, EncoderOn::Direct3D11, GrabSync::Query),
    (PixelFormat::Argb, EncoderOn::Direct3D11, GrabSync::Sleep),
    (PixelFormat::Argb, EncoderOn::Direct3D9, GrabSync::None),
    (PixelFormat::Nv12, EncoderOn::Direct3D9, GrabSync::Query),
    (PixelFormat::Nv12, EncoderOn::Direct3D9, GrabSync::None),
];

#[test]
#[ignore = "grabs the primary monitor with NvFBC for about forty seconds; run by hand"]
fn grab_sync_cost() {
    let device = Device::create().expect("create the Direct3D 11 device");
    for (format, encoder, mode) in CASES {
        println!("\n== {format:?}, encoder on {encoder:?}, {mode:?}");
        match measure(&device, format, encoder, mode) {
            Ok(line) => println!("{line}"),
            Err(error) => println!("could not measure: {error}"),
        }
    }
    println!("\n== probe: query issued after the grab, never flushed");
    match probe_unflushed(&device) {
        Ok(line) => println!("{line}"),
        Err(error) => println!("could not probe: {error}"),
    }
}

/// Opens a capture of the primary monitor that counts cycles.
fn open(
    device: &Device,
    format: PixelFormat,
    encoder: EncoderOn,
    sync: GrabSync,
) -> Result<Capture, String> {
    let setup = Setup {
        switches: Switches::DEFAULT,
        format,
        encoder,
        sync,
        stats: Some(Stats::new()),
    };
    let mut capture = Capture::start_with(device, &Target::primary_monitor(), false, setup)
        .map_err(|error| error.to_string())?;
    super::Source::hand_over_surfaces(&mut capture);
    Ok(capture)
}

/// Grabs at 240 frames a second in one mode and reports the counts.
fn measure(
    device: &Device,
    format: PixelFormat,
    encoder: EncoderOn,
    mode: GrabSync,
) -> Result<String, String> {
    let mut capture = open(device, format, encoder, mode)?;
    let (width, height) = capture.size;
    let mut next = Instant::now();
    let mut process_start = 0;
    for frame in 0..WARMUP + FRAMES {
        if frame == WARMUP {
            capture.stats = Some(Stats::new());
            process_start = process_cycles();
        }
        super::Source::take(&mut capture, device, None, width, height)
            .map_err(|error| error.to_string())?;
        next += FRAME_PERIOD;
        if let Some(rest) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
    }
    let process = (process_cycles() - process_start) / u64::from(FRAMES);
    let stats = capture.stats.take().expect("the capture counts");
    Ok(format!(
        "{}\nprocess cycles per frame (every thread): {process}",
        stats.summary(capture.sync)
    ))
}

/// Follows grabs with the query but no flush and reports how many landed within
/// [`PROBE_PATIENCE`] when looked at without flushing.
fn probe_unflushed(device: &Device) -> Result<String, String> {
    let mut capture = open(
        device,
        PixelFormat::Argb,
        EncoderOn::Direct3D11,
        GrabSync::Query,
    )?;
    let mut landed = 0;
    for _ in 0..PROBE_FRAMES {
        let started = Instant::now();
        let session = capture.session.as_ref().ok_or("the session was lost")?;
        let (status, _) = session.grab_frame(capture.next, capture.size, capture.switches);
        if status != super::Status::SUCCESS {
            return Err(format!("grab failed: {status}"));
        }
        let query = &capture.slots[capture.next as usize].done;
        // SAFETY: marks the end of the grab's work on the device.
        unsafe { query.Issue(D3DISSUE_END) }.map_err(|error| error.to_string())?;
        while started.elapsed() < PROBE_PATIENCE {
            // SAFETY: a live event query, which takes no data buffer; no flag, so nothing is
            // sent to the GPU by looking.
            let result = unsafe {
                (Interface::vtable(query).GetData)(query.as_raw(), ptr::null_mut(), 0, 0)
            };
            if result == S_OK {
                landed += 1;
                break;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        capture.next = (capture.next + 1) % super::SLOT_COUNT;
        if let Some(rest) = (started + FRAME_PERIOD).checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
    }
    Ok(format!(
        "{landed} of {PROBE_FRAMES} unflushed grabs landed within {PROBE_PATIENCE:?}"
    ))
}

/// Cycles every thread of this process, the driver's included, has run so far.
fn process_cycles() -> u64 {
    let mut cycles = 0;
    // SAFETY: the pseudo-handle of this process and one integer to write.
    unsafe { QueryProcessCycleTime(GetCurrentProcess(), &raw mut cycles) }
        .expect("query the process's cycles");
    cycles
}
