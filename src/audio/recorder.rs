//! The audio thread of a session: it opens the default output device's loopback capture, then
//! every few packets reads what has arrived, places it on the run and hands it to the buffer
//! as 16-bit PCM, until the session stops it. It does not encode: a save encodes the clip's
//! span. It keeps the sound on the shared clock through everything a device does: a gap is
//! left wherever nothing arrived, for a save to fill with silence, whether nothing played (a
//! quiet device: Windows sends a loopback capture nothing then), the device was lost or a
//! reopen took its time; a packet marked as a discontinuity is simply stamped later than the
//! run expects, which reads as a gap.

use super::{
    CHANNELS, SAMPLE_RATE, aac,
    loopback::{self, Com, Devices, Loopback, OpenError, Packet, Signal, Wake, Watch},
    pcm::{self, Layout},
    timeline::Timeline,
};
use crate::{
    log,
    replay::{AudioFormat, CLOCK_RATE, Live},
    sys::{self, clock},
};
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{Foundation::HANDLE, Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED};

/// How often the thread reads the open capture: six of the audio engine's 10 ms packets at a
/// time, so it wakes a sixth as often as a capture that signals every packet would wake it,
/// for 50 ms more latency, which a replay buffer never notices. The capture buffer holds 200 ms,
/// so a late read loses nothing.
const READ_INTERVAL: Duration = Duration::from_millis(60);
/// The longest the thread sleeps while no device is open, before it fills in silence and tries
/// the device again.
const WAKE_INTERVAL: Duration = Duration::from_millis(100);
/// How far behind the present silence is filled in while nothing arrives, in clock ticks:
/// 500 ms, far more than the capture's latency, so a packet that is merely late is not taken
/// for a gap. A save fills the last moments with silence too, so nothing is lost by waiting.
const SILENCE_LAG: i64 = CLOCK_RATE / 2;
/// How often a device that could not be opened is tried again.
const REOPEN_INTERVAL: Duration = Duration::from_secs(1);
/// How often the thread asks which output device is the default, when Windows could not be
/// asked to say so itself.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// The sound capture of a running session; dropping it stops the capture and waits for its
/// thread, which takes no longer than closing the device.
pub(crate) struct Capture {
    stop: Arc<Signal>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Starts recording the system's sound into `live` on a thread of its own, or logs why it
/// cannot.
pub(crate) fn start(live: &Arc<Live>) -> Option<Capture> {
    let stop = match Signal::new() {
        Ok(signal) => Arc::new(signal),
        Err(error) => {
            log::warning(format!("Sound is not recorded: {error}"));
            return None;
        }
    };
    let (live, signal) = (Arc::clone(live), Arc::clone(&stop));
    match thread::Builder::new()
        .name("audio".to_owned())
        .spawn(move || record(&live, &signal))
    {
        Ok(join) => Some(Capture {
            stop,
            join: Some(join),
        }),
        Err(error) => {
            log::warning(format!(
                "Sound is not recorded: the audio thread could not start ({error})"
            ));
            None
        }
    }
}

/// Records sound into `live` until `stop` is set.
fn record(live: &Live, stop: &Signal) {
    let Some(mut pipeline) = Pipeline::open(live) else {
        return;
    };
    let mut device = Device::new(watch_default(&pipeline.devices));
    let mut placed = false;
    loop {
        if !placed {
            placed = sys::thread::keep_off_capture_core();
        }
        device.update(&pipeline.devices);
        if let Wake::Stop = loopback::wait(stop, device.wake(), device.interval()) {
            break;
        }
        if let Err(error) = device.read(&mut pipeline.track) {
            device.lost(&error);
        }
        pipeline.track.catch_up(clock::now() - SILENCE_LAG);
        pipeline.track.flush();
    }
    log::info("Sound recording stopped");
}

/// What the audio thread records with. The fields drop in this order: the device list releases
/// its COM objects before COM is shut down.
struct Pipeline<'a> {
    track: Track<'a>,
    devices: Devices,
    _com: Com,
}

impl<'a> Pipeline<'a> {
    /// Starts COM on this thread and opens the device list, or logs which of them keeps the
    /// session's sound from being recorded.
    fn open(live: &'a Live) -> Option<Self> {
        let com = started(Com::start(), "COM is unavailable")?;
        let devices = started(Devices::new(), "the audio devices could not be listed")?;
        Some(Self {
            track: Track::open(live),
            devices,
            _com: com,
        })
    }
}

/// What `result` started, or `None` once the reason sound is not recorded, `what`, is logged.
fn started<T>(result: windows::core::Result<T>, what: &str) -> Option<T> {
    result
        .inspect_err(|error| log::warning(format!("Sound is not recorded: {what} ({error})")))
        .ok()
}

/// Asks Windows to report changes of the default output device, or logs that the device will
/// be polled instead.
fn watch_default(devices: &Devices) -> Option<Watch> {
    devices
        .watch()
        .inspect_err(|error| {
            log::warning(format!(
                "Windows will not report audio device changes ({error}); checking every {} s instead",
                DEFAULT_CHECK_INTERVAL.as_secs()
            ));
        })
        .ok()
}

/// The capture of the default output device, opened again whenever it fails or another device
/// becomes the default.
struct Device {
    loopback: Option<Loopback>,
    /// Windows' word on default-device changes, when it could be asked; otherwise the device
    /// is polled.
    watch: Option<Watch>,
    /// When the next attempt to open the device, or the next look at which one is the default,
    /// is due.
    due: Option<Instant>,
    /// Whether a device was open before, so the next one is logged as a change.
    opened_before: bool,
    /// Whether a failure to open was logged since the last success, so it is said once.
    reported: bool,
}

impl Device {
    fn new(watch: Option<Watch>) -> Self {
        Self {
            loopback: None,
            watch,
            due: None,
            opened_before: false,
            reported: false,
        }
    }

    /// How long to sleep before the next read: a few packets' worth while a device is open,
    /// longer while there is none.
    fn interval(&self) -> Duration {
        if self.loopback.is_some() {
            READ_INTERVAL
        } else {
            WAKE_INTERVAL
        }
    }

    /// The event that ends a sleep early when the default device changes.
    fn wake(&self) -> Option<HANDLE> {
        self.watch.as_ref().map(Watch::event)
    }

    /// Opens the device when an attempt is due, and closes it when another one has become the
    /// default, so the next update opens that one.
    fn update(&mut self, devices: &Devices) {
        let now = Instant::now();
        if self.watch.as_ref().is_some_and(Watch::changed) {
            self.switch(devices, now);
            return;
        }
        if self.due.is_some_and(|due| now < due) {
            return;
        }
        let Some(open) = &self.loopback else {
            self.open(devices, now);
            return;
        };
        if self.watch.is_none() && devices.default_id().is_some_and(|id| id != open.id()) {
            self.switch(devices, now);
            return;
        }
        self.due = Some(now + DEFAULT_CHECK_INTERVAL);
    }

    /// Moves to the device that has become the default.
    fn switch(&mut self, devices: &Devices, now: Instant) {
        if self.loopback.take().is_some() {
            log::info("The default audio output changed; switching to it");
        }
        self.open(devices, now);
    }

    /// Opens the default output device, logging the result once.
    fn open(&mut self, devices: &Devices, now: Instant) {
        match devices.open() {
            Ok(loopback) => {
                log::info(format!(
                    "Capturing sound from {}{}",
                    loopback.name(),
                    if self.opened_before { " again" } else { "" }
                ));
                self.loopback = Some(loopback);
                self.opened_before = true;
                self.reported = false;
                self.due = Some(now + DEFAULT_CHECK_INTERVAL);
            }
            Err(error) => {
                if !self.reported {
                    match error {
                        OpenError::NoDevice => log::info(
                            "No audio output device; clips get silence until one is connected",
                        ),
                        OpenError::Windows(..) => log::warning(format!(
                            "The audio output could not be captured ({error}); trying again every second"
                        )),
                    }
                    self.reported = true;
                }
                self.due = Some(now + REOPEN_INTERVAL);
            }
        }
    }

    /// Closes a capture that failed with `error`; the next update opens the device again.
    fn lost(&mut self, error: &windows::core::Error) {
        if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            log::info("The audio output device went away; reopening");
        } else {
            log::warning(format!("Sound capture failed ({error}); reopening"));
        }
        self.loopback = None;
        self.due = None;
    }

    /// Hands every packet waiting in the open capture to `track`.
    fn read(&mut self, track: &mut Track<'_>) -> windows::core::Result<()> {
        let Some(loopback) = self.loopback.as_mut() else {
            return Ok(());
        };
        let layout = loopback.layout();
        loopback.read(|packet| {
            track.packet(packet, layout);
            Ok(())
        })
    }
}

/// The session's run of sound: where it stands on the clock, the number of the next sample,
/// and the samples converted since they were last handed to the buffer.
struct Track<'a> {
    live: &'a Live,
    timeline: Timeline,
    /// The number the run's next sample gets, silence included.
    next: u64,
    /// Interleaved samples converted since the last hand-over, all following each other.
    pending: Vec<i16>,
    /// The number of the first sample in `pending`.
    pending_start: u64,
}

impl<'a> Track<'a> {
    /// Records the stream's format in `live`, with the run starting now.
    fn open(live: &'a Live) -> Self {
        let anchor = clock::now();
        live.set_audio(AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            anchor,
            priming: aac::PRIMING,
        });
        Self {
            live,
            timeline: Timeline::new(SAMPLE_RATE, anchor),
            next: 0,
            pending: Vec::new(),
            pending_start: 0,
        }
    }

    /// Places `packet`, whose samples are laid out as `layout` says, on the run and converts
    /// it. A silent packet is a gap, which a save encodes as silence.
    fn packet(&mut self, packet: &Packet<'_>, layout: Layout) {
        let placement = self.timeline.place(packet.time, packet.frames);
        self.skip(placement.silence);
        let kept = packet.frames - placement.skip;
        let Some(data) = packet.data else {
            return self.skip(u64::from(kept));
        };
        if self.pending.is_empty() {
            self.pending_start = self.next;
        }
        let length = self.pending.len() + kept as usize * usize::from(CHANNELS);
        let skipped_bytes = placement.skip as usize * layout.frame_bytes();
        pcm::append_stereo(
            &mut self.pending,
            &data[skipped_bytes.min(data.len())..],
            layout,
        );
        // The run counts the frames the packet claims, whatever its data holds.
        self.pending.resize(length, 0);
        self.next += u64::from(kept);
    }

    /// Leaves the run empty up to clock time `time`, if nothing has arrived to fill it.
    fn catch_up(&mut self, time: i64) {
        let silence = self.timeline.catch_up(time);
        self.skip(silence);
    }

    /// Passes over `samples` of silence: they are not kept, and a save encodes silence there.
    fn skip(&mut self, samples: u64) {
        if samples > 0 {
            self.flush();
            self.next += samples;
        }
    }

    /// Hands the samples converted since the last time to the session's buffer.
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.live
                .push_audio(self.pending_start, Arc::from(&self.pending[..]));
            self.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Codec,
        replay::{Matrix, Packet, Stream},
    };

    /// Records ten seconds of whatever the machine plays and checks that the frames keep pace
    /// with the clock. Set `HINDSIGHT_AUDIO_OUTPUT` to a file to keep the sound as ADTS, e.g.
    /// to listen to it or measure its loudness with ffmpeg. Run by hand:
    /// `cargo test records_the_system_sound -- --ignored --nocapture`.
    #[test]
    #[ignore = "records the machine's sound for ten seconds"]
    fn records_the_system_sound() {
        let live = Arc::new(Live::new(1_000));
        live.set_stream(Stream {
            codec: Codec::H264,
            width: 64,
            height: 64,
            frame_rate: 60,
            matrix: Matrix::Bt709,
        });
        live.set_origin(clock::now());
        let packet = |index| Packet {
            index,
            keyframe: true,
            data: Box::new([0]),
        };
        live.push(packet(0), true);
        let capture = start(&live).unwrap();
        thread::sleep(Duration::from_secs(10));
        live.push(packet(599), true);
        drop(capture);

        for entry in log::since(0) {
            println!("{}", entry.line());
        }
        let audio = crate::audio::encode(&live.clip().unwrap().audio.unwrap()).unwrap();
        println!(
            "{} frames for 10 s of video, lead {}",
            audio.frames.len(),
            audio.lead
        );
        // Ten seconds at 48 kHz are 468.75 frames, plus the one in front and the last partial.
        assert!((469..=471).contains(&audio.frames.len()));
        if let Some(path) = std::env::var_os("HINDSIGHT_AUDIO_OUTPUT") {
            let mut adts = Vec::new();
            for frame in &audio.frames {
                let length = frame.len() + 7;
                let [_, _, middle, low] = u32::try_from(length).unwrap().to_be_bytes();
                // AAC-LC, 48 kHz, two channels, then the 13-bit frame length.
                adts.extend([
                    0xFF,
                    0xF1,
                    0x4C,
                    0x80 | (middle >> 3),
                    (middle << 5) | (low >> 3),
                    ((low & 0x07) << 5) | 0x1F,
                    0xFC,
                ]);
                adts.extend_from_slice(frame);
            }
            std::fs::write(path, adts).unwrap();
        }
    }
}
