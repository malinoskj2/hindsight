//! How a capture makes sure NvFBC's grab into a surface has landed before NVENC reads the
//! surface, and the per-call cycle counts that compare the ways of doing it.
//!
//! The grab is a blit the driver queues on the capture's Direct3D 9Ex device. With the
//! default ARGB surfaces NVENC reads them through the encoder's Direct3D 11 device, and
//! Direct3D 9Ex shares surfaces unsynchronized: Microsoft's "Surface sharing between Windows
//! graphics APIs" says that to share reading and writing of a Direct3D 9Ex shared surface "you
//! must add manual synchronization to the application itself", and keyed mutexes exist only for
//! DXGI 1.1 resources, which a Direct3D 9Ex surface is not. NVIDIA's own two-device sample
//! (`NvFBCDX9NvEncSharedSurface`) likewise reads a pixel of the grab back to the CPU after
//! every grab before the encoding device may touch it. So the ARGB path keeps a CPU-side check:
//! an event query issued after the grab and flushed at once, and looked at again when the
//! surface is handed over a tick later.
//!
//! `HINDSIGHT_GRAB_SYNC` chooses how, unset or unknown leaving the default:
//!
//! - `query`, the default: issue the query and flush it after the grab; at handover look until
//!   it reports done, yielding the processor between looks.
//! - `sleep`: the same query, but a handover that finds the grab unfinished sleeps a tenth of a
//!   millisecond between looks instead of yielding, so the rare late grab (a busy GPU reaches it
//!   after the game's work) costs a few looks rather than a spinning thread.
//! - `none` (also read as `nv12same`): no query and no CPU wait at all, when NVENC runs on
//!   NvFBC's own Direct3D 9Ex device (`HINDSIGHT_NVFBC_ENCODER=d3d9`, or
//!   `HINDSIGHT_NVFBC_FORMAT=nv12`, which implies it). A device's work on one resource is
//!   carried out in the order it was submitted, so the grab's blit is done before NVENC's read
//!   of the same surface begins: NVIDIA's single-device sample `NvFBCDX9NvEnc` (Capture SDK,
//!   `samples/NvFBC/NvFBCDX9NvEnc/NvFBCDX9NvEnc.cpp`) grabs ARGB with `NVFBC_TODX9VID_NOWAIT`
//!   into plain Direct3D 9 surfaces registered with an NVENC session on the same device, and
//!   calls `nvEncMapInputResource` and `nvEncEncodePicture` on the surface straight after the
//!   grab, with no query or wait in between. NVENC's own submission sends the grab's blit to
//!   the GPU, so no explicit flush is needed either. With NVENC on the Direct3D 11 device the
//!   two devices make this unsafe, and the mode falls back to `query`. Without the query, a
//!   driver that stopped completing grabs is no longer noticed here but by NVENC's own wait.
//!
//! With `HINDSIGHT_GRAB_SYNC_STATS=1` the capture logs every ten seconds the thread cycles it
//! spent per frame in the grab call, issuing the query, flushing it, and waiting at handover.

use super::{CaptureError, failed};
use crate::log;
use std::{
    ptr,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::S_OK,
        Graphics::Direct3D9::{D3DGETDATA_FLUSH, D3DISSUE_END, IDirect3DQuery9},
        System::{Threading::GetCurrentThread, WindowsProgramming::QueryThreadCycleTime},
    },
    core::Interface,
};

/// The variable naming the way grabs are followed; see [`GrabSync`].
const MODE_VARIABLE: &str = "HINDSIGHT_GRAB_SYNC";
/// The variable that turns the cycle counts on; see [`Stats`].
const STATS_VARIABLE: &str = "HINDSIGHT_GRAB_SYNC_STATS";
/// How long a grab may take on the GPU before the driver is judged to have stopped.
const GRAB_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the `sleep` mode sleeps between looks at an unfinished grab: a small part of a
/// 240 fps tick, and long enough that a late grab costs a handful of looks.
const SLEEP_STEP: Duration = Duration::from_micros(100);
/// How often the cycle counts are logged.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// How a grab is known to have landed before its surface is handed over; the module
/// documentation gives each mode's reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GrabSync {
    Query,
    Sleep,
    None,
}

impl GrabSync {
    /// What the environment asks for.
    pub(super) fn from_environment() -> Self {
        Self::parse(std::env::var(MODE_VARIABLE).ok().as_deref())
    }

    /// The mode a value of the variable names, without regard to case or surrounding
    /// whitespace; anything else leaves the default.
    fn parse(value: Option<&str>) -> Self {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("sleep") => Self::Sleep,
            Some("none" | "nv12same") => Self::None,
            _ => Self::Query,
        }
    }

    /// The mode that is safe given whether NVENC reads the surfaces on the capture's own device
    /// (`same_device`), which skipping the query needs.
    pub(super) fn effective(self, same_device: bool) -> Self {
        match self {
            Self::None if !same_device => Self::Query,
            mode => mode,
        }
    }

    /// The name the variable takes for the mode.
    const fn name(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Sleep => "sleep",
            Self::None => "none",
        }
    }

    /// Logs the mode the capture runs with when the variable asked for anything but the
    /// default, saying when it could not be had.
    pub(super) fn report(asked: Self, effective: Self) {
        if asked == Self::Query {
            return;
        }
        if asked == effective {
            log::info(format!("NvFBC grab sync: {}", effective.name()));
        } else {
            log::info(format!(
                "NvFBC grab sync: {} needs NVENC on NvFBC's Direct3D 9 device (HINDSIGHT_NVFBC_ENCODER=d3d9 or HINDSIGHT_NVFBC_FORMAT=nv12); using {}",
                asked.name(),
                effective.name()
            ));
        }
    }

    /// Whether grabs are followed by a query at all.
    const fn uses_query(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Follows a grab just started on the device: queues `query` after it and sends both to
    /// the GPU, unless the mode needs no query.
    pub(super) fn follow(
        self,
        query: &IDirect3DQuery9,
        stats: &mut Option<Stats>,
    ) -> Result<(), CaptureError> {
        if !self.uses_query() {
            return Ok(());
        }
        // SAFETY: marks the end of the grab's work on the device.
        timed(stats, Call::Issue, || unsafe { query.Issue(D3DISSUE_END) })
            .map_err(failed("follow the NvFBC grab"))?;
        // Left in Direct3D 9's command buffer, the grab's work and the query would only reach
        // the GPU at the next tick's first look, which would then wait as long as a GPU busy
        // with a game takes to get to them: about 0.8 ms of spinning per frame.
        timed(stats, Call::Flush, || is_done(query))?;
        Ok(())
    }

    /// Makes sure the grab `query` follows has landed before its surface is handed over,
    /// waiting for it when the mode needs to.
    pub(super) fn settle(
        self,
        query: &IDirect3DQuery9,
        stats: &mut Option<Stats>,
    ) -> Result<(), CaptureError> {
        if let Some(stats) = stats {
            stats.frames += 1;
        }
        if !self.uses_query() {
            return Ok(());
        }
        let late = timed(stats, Call::Wait, || wait_for(query, self))?;
        if let (true, Some(stats)) = (late, stats.as_mut()) {
            stats.late += 1;
        }
        Ok(())
    }
}

/// Whether the work before `query` has finished on the GPU; submits any of it still waiting in
/// the device's command buffer.
fn is_done(query: &IDirect3DQuery9) -> Result<bool, CaptureError> {
    // The method's wrapper reports S_FALSE ("not yet") as success, so it is called directly.
    // SAFETY: a live query; an event query takes no data buffer.
    let result = unsafe {
        (Interface::vtable(query).GetData)(query.as_raw(), ptr::null_mut(), 0, D3DGETDATA_FLUSH)
    };
    if result.is_err() {
        return Err(failed("wait for the NvFBC grab")(result.into()));
    }
    Ok(result == S_OK)
}

/// Waits until the work before `query` has finished on the GPU, which it usually has: the
/// grab was sent to the GPU a whole frame earlier. Between looks the `sleep` mode sleeps and
/// the others yield. Reports whether the first look found it unfinished.
fn wait_for(query: &IDirect3DQuery9, mode: GrabSync) -> Result<bool, CaptureError> {
    if is_done(query)? {
        return Ok(false);
    }
    let deadline = Instant::now() + GRAB_TIMEOUT;
    loop {
        if mode == GrabSync::Sleep {
            std::thread::sleep(SLEEP_STEP);
        } else {
            std::thread::yield_now();
        }
        if is_done(query)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            // A wedged driver is usually mended by a fresh session, so this is not permanent.
            return Err(CaptureError::Restart(
                "the NVIDIA driver stopped completing NvFBC grabs",
            ));
        }
    }
}

/// The calls whose thread cycles [`Stats`] adds up.
#[derive(Clone, Copy)]
pub(super) enum Call {
    /// `NvFBCToDx9VidGrabFrame`.
    Grab,
    /// Issuing the query after the grab.
    Issue,
    /// The look right after issuing, which flushes the grab and the query to the GPU.
    Flush,
    /// Every look at handover, the waiting between them included.
    Wait,
}

/// The thread cycles the capture spent in each [`Call`] since the last report, kept only when
/// `HINDSIGHT_GRAB_SYNC_STATS=1`.
#[derive(Debug)]
pub(super) struct Stats {
    since: Instant,
    /// Frames handed over.
    pub(super) frames: u64,
    pub(super) grab: u64,
    pub(super) issue: u64,
    pub(super) flush: u64,
    pub(super) wait: u64,
    /// Handovers whose first look found the grab unfinished.
    pub(super) late: u64,
}

impl Stats {
    /// Counts from zero, if the environment asks for them.
    pub(super) fn from_environment() -> Option<Self> {
        let value = std::env::var(STATS_VARIABLE).ok();
        (value.as_deref().map(str::trim) == Some("1")).then(Self::new)
    }

    /// Counts from zero.
    pub(super) fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            grab: 0,
            issue: 0,
            flush: 0,
            wait: 0,
            late: 0,
        }
    }

    /// Adds `cycles` to `call`'s count.
    fn add(&mut self, call: Call, cycles: u64) {
        let count = match call {
            Call::Grab => &mut self.grab,
            Call::Issue => &mut self.issue,
            Call::Flush => &mut self.flush,
            Call::Wait => &mut self.wait,
        };
        *count += cycles;
    }

    /// The counts per frame handed over, as one line.
    pub(super) fn summary(&self, mode: GrabSync) -> String {
        let per_frame = |cycles: u64| cycles / self.frames.max(1);
        format!(
            "NvFBC grab sync {}: {} frames, thread cycles per frame: grab {}, issue {}, flush {}, handover wait {} (sync total {}); {} handovers found the grab unfinished",
            mode.name(),
            self.frames,
            per_frame(self.grab),
            per_frame(self.issue),
            per_frame(self.flush),
            per_frame(self.wait),
            per_frame(self.issue + self.flush + self.wait),
            self.late,
        )
    }

    /// Logs the counts and starts again once a report is due.
    pub(super) fn report_when_due(stats: &mut Option<Self>, mode: GrabSync) {
        let Some(counts) = stats else {
            return;
        };
        if counts.since.elapsed() < REPORT_EVERY {
            return;
        }
        log::info(counts.summary(mode));
        *counts = Self::new();
    }
}

/// Runs `body`, adding the thread cycles it took to `call`'s count when cycles are counted.
pub(super) fn timed<T>(stats: &mut Option<Stats>, call: Call, body: impl FnOnce() -> T) -> T {
    let Some(stats) = stats else {
        return body();
    };
    let start = thread_cycles();
    let result = body();
    stats.add(call, thread_cycles().saturating_sub(start));
    result
}

/// Thread cycles the calling thread has run so far; zero should the count be unavailable,
/// which only spoils a measurement.
fn thread_cycles() -> u64 {
    let mut cycles = 0;
    // SAFETY: the pseudo-handle of the calling thread and one integer to write.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &raw mut cycles) };
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_loosely_and_default_to_the_query() {
        assert_eq!(GrabSync::parse(None), GrabSync::Query);
        assert_eq!(GrabSync::parse(Some("")), GrabSync::Query);
        assert_eq!(GrabSync::parse(Some("keyedmutex")), GrabSync::Query);
        assert_eq!(GrabSync::parse(Some(" Sleep ")), GrabSync::Sleep);
        assert_eq!(GrabSync::parse(Some("NV12SAME")), GrabSync::None);
        assert_eq!(GrabSync::parse(Some("none")), GrabSync::None);
    }

    #[test]
    fn skipping_the_query_needs_one_device() {
        assert_eq!(GrabSync::None.effective(false), GrabSync::Query);
        assert_eq!(GrabSync::None.effective(true), GrabSync::None);
        assert_eq!(GrabSync::Sleep.effective(false), GrabSync::Sleep);
        assert!(!GrabSync::None.uses_query());
        assert!(GrabSync::Sleep.uses_query());
    }

    #[test]
    fn counts_are_per_frame_handed_over() {
        let mut stats = Stats::new();
        stats.frames = 4;
        stats.issue = 400;
        stats.flush = 4_000;
        stats.wait = 800;
        let line = stats.summary(GrabSync::Query);
        assert!(line.contains("issue 100, flush 1000, handover wait 200 (sync total 1300)"));
    }
}
