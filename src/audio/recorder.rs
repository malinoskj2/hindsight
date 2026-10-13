//! The session's audio thread: reads the loopback capture on a timer, keeps the sound on the
//! shared clock and hands it to the buffer as 16-bit PCM; gaps are left for a save to fill.

use super::{
    CHANNELS, SAMPLE_RATE, aac,
    loopback::{self, Devices, Loopback, OpenError, Packet, Signal, Wake, Watch},
    pcm::{self, Layout},
    timeline::{Frames, Ticks, Timeline},
};
use crate::{
    log,
    replay::{AudioFormat, CLOCK_RATE, Live},
    sys::{self, clock, com::Apartment},
};
use std::{
    any::Any,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{Foundation::HANDLE, Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED};

/// Six 10 ms engine packets per read; the 200 ms capture buffer covers a late one.
const READ_INTERVAL: Duration = Duration::from_millis(60);
const WAKE_INTERVAL: Duration = Duration::from_millis(100);
/// Silence is filled in this far behind the present, so a late packet is not taken for a gap.
const SILENCE_LAG: Ticks = Ticks(CLOCK_RATE / 2);
const REOPEN_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct Capture {
    stop: Arc<Signal>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(join) = self.join.take()
            && let Err(payload) = join.join()
        {
            log::error(format!(
                "The audio thread panicked: {}",
                panic_message(&*payload)
            ));
        }
    }
}

/// The text a panic was raised with, when it was a string.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

pub(crate) fn start(live: &Arc<Live>) -> Option<Capture> {
    let stop = match Signal::latch() {
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

fn record(live: &Live, stop: &Signal) {
    let Some(mut pipeline) = Pipeline::open(live) else {
        return;
    };
    let Pipeline {
        track,
        device,
        devices,
        ..
    } = &mut pipeline;
    let mut placed = false;
    let mut changed = false;
    loop {
        if !placed && let Some(capture) = live.capture_processor() {
            sys::thread::keep_off(capture);
            placed = true;
        }
        device.update(devices, changed);
        // A failed wait would fail again at once; stopping beats spinning on it.
        let wake = match loopback::wait(stop, device.wake(), device.interval()) {
            Ok(Wake::Stop) => break,
            Ok(wake) => wake,
            Err(error) => {
                log::error(format!("The audio thread could not wait ({error})"));
                break;
            }
        };
        changed = matches!(wake, Wake::Changed);
        if let Err(error) = device.read(track) {
            device.lost(&error);
        }
        track.catch_up(Ticks(clock::now()).saturating_sub(SILENCE_LAG));
        track.flush();
    }
    log::info("Sound recording stopped");
}

/// Fields drop in this order: the open device, its watch and the device list release their COM
/// objects before COM shuts down.
struct Pipeline<'a> {
    track: Track<'a>,
    device: Device,
    devices: Devices,
    _com: Apartment,
}

impl<'a> Pipeline<'a> {
    fn open(live: &'a Live) -> Option<Self> {
        let com = started(Apartment::multi_threaded(), "COM is unavailable")?;
        let devices = started(Devices::new(), "the audio devices could not be listed")?;
        Some(Self {
            track: Track::open(live),
            device: Device::new(watch_default(&devices)),
            devices,
            _com: com,
        })
    }
}

fn started<T>(result: windows::core::Result<T>, what: &str) -> Option<T> {
    result
        .inspect_err(|error| log::warning(format!("Sound is not recorded: {what} ({error})")))
        .ok()
}

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

struct Device {
    state: DeviceState,
    /// `None` means the default device is polled.
    watch: Option<Watch>,
    /// When to next check the default device or try to open one; `None` is now.
    due: Option<Instant>,
}

enum DeviceState {
    Open(Loopback),
    Closed {
        /// A device was open before, so opening one is logged as a return.
        opened_before: bool,
        /// A failure to open has been logged since the last success, so another is not.
        reported: bool,
    },
}

impl DeviceState {
    /// After a device that was open closes.
    const CLOSED_AFTER_OPEN: Self = Self::Closed {
        opened_before: true,
        reported: false,
    };
}

impl Device {
    const fn new(watch: Option<Watch>) -> Self {
        Self {
            state: DeviceState::Closed {
                opened_before: false,
                reported: false,
            },
            watch,
            due: None,
        }
    }

    const fn interval(&self) -> Duration {
        match self.state {
            DeviceState::Open(_) => READ_INTERVAL,
            DeviceState::Closed { .. } => WAKE_INTERVAL,
        }
    }

    fn wake(&self) -> Option<HANDLE> {
        self.watch.as_ref().map(Watch::event)
    }

    /// `changed` is true when the watch reported a new default output since the last update.
    fn update(&mut self, devices: &Devices, changed: bool) {
        let now = Instant::now();
        if changed {
            self.switch(devices, now);
            return;
        }
        if self.due.is_some_and(|due| now < due) {
            return;
        }
        let DeviceState::Open(open) = &self.state else {
            self.open(devices, now);
            return;
        };
        if self.watch.is_none() && devices.default_id().is_some_and(|id| id != open.id()) {
            self.switch(devices, now);
            return;
        }
        self.due = Some(now + DEFAULT_CHECK_INTERVAL);
    }

    fn switch(&mut self, devices: &Devices, now: Instant) {
        if let DeviceState::Open(_) = self.state {
            log::info("The default audio output changed; switching to it");
            self.state = DeviceState::CLOSED_AFTER_OPEN;
        }
        self.open(devices, now);
    }

    fn open(&mut self, devices: &Devices, now: Instant) {
        let DeviceState::Closed {
            opened_before,
            reported,
        } = &mut self.state
        else {
            return;
        };
        match devices.open() {
            Ok(loopback) => {
                log::info(format!(
                    "Capturing sound from {}{}",
                    loopback.name(),
                    if *opened_before { " again" } else { "" }
                ));
                self.state = DeviceState::Open(loopback);
                self.due = Some(now + DEFAULT_CHECK_INTERVAL);
            }
            Err(error) => {
                if !*reported {
                    match &error {
                        OpenError::NoDevice => log::info(
                            "No audio output device; clips get silence until one is connected",
                        ),
                        OpenError::Windows(_, cause) => log::warning(format!(
                            "The audio output could not be captured ({error}: {cause}); trying again every second"
                        )),
                    }
                    *reported = true;
                }
                self.due = Some(now + REOPEN_INTERVAL);
            }
        }
    }

    fn lost(&mut self, error: &windows::core::Error) {
        if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            log::info("The audio output device went away; reopening");
        } else {
            log::warning(format!("Sound capture failed ({error}); reopening"));
        }
        self.state = DeviceState::CLOSED_AFTER_OPEN;
        self.due = None;
    }

    fn read(&mut self, track: &mut Track<'_>) -> windows::core::Result<()> {
        let DeviceState::Open(loopback) = &mut self.state else {
            return Ok(());
        };
        let layout = loopback.layout();
        loopback.read(|packet| {
            track.packet(packet, layout);
            Ok(())
        })
    }
}

/// Where the track hands finished runs of sound: the live buffer, or a test's list.
trait Sink {
    /// Interleaved samples starting at frame `start`.
    fn push_audio(&self, start: u64, data: Arc<[i16]>);
}

impl Sink for Live {
    fn push_audio(&self, start: u64, data: Arc<[i16]>) {
        Live::push_audio(self, start, data);
    }
}

struct Track<'a> {
    sink: &'a dyn Sink,
    timeline: Timeline,
    /// Number of the next frame, silence included.
    next: Frames,
    /// Interleaved samples, `CHANNELS` to a frame, not yet handed to the buffer.
    pending: Vec<i16>,
    /// Number of the first frame in `pending`.
    pending_start: Frames,
}

impl<'a> Track<'a> {
    fn open(live: &'a Live) -> Self {
        let anchor = clock::now();
        live.set_audio(AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            anchor,
            priming: aac::PRIMING,
        });
        Self::new(live, Ticks(anchor))
    }

    fn new(sink: &'a dyn Sink, anchor: Ticks) -> Self {
        Self {
            sink,
            timeline: Timeline::new(SAMPLE_RATE, anchor),
            next: Frames::ZERO,
            pending: Vec::new(),
            pending_start: Frames::ZERO,
        }
    }

    fn packet(&mut self, packet: &Packet<'_>, layout: Layout) {
        let frames = Frames::from(packet.frames);
        let placement = self.timeline.place(packet.time, frames);
        self.insert_silence(placement.silence);
        let kept = frames - placement.overlap;
        let Some(data) = packet.data else {
            return self.insert_silence(kept);
        };
        if self.pending.is_empty() {
            self.pending_start = self.next;
        }
        let length = self.pending.len() + count(kept).saturating_mul(usize::from(CHANNELS.get()));
        let dropped_bytes = count(placement.overlap).saturating_mul(layout.frame_bytes());
        pcm::append_stereo(
            &mut self.pending,
            &data[dropped_bytes.min(data.len())..],
            layout,
        );
        // Pad to the frames the packet claims, whatever its data holds.
        self.pending.resize(length, 0);
        self.next += kept;
    }

    fn catch_up(&mut self, time: Ticks) {
        let silence = self.timeline.catch_up(time);
        self.insert_silence(silence);
    }

    fn insert_silence(&mut self, frames: Frames) {
        if frames > Frames::ZERO {
            self.flush();
            self.next += frames;
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.sink
                .push_audio(self.pending_start.0, Arc::from(&self.pending[..]));
            self.pending.clear();
        }
    }
}

/// `frames` as a `usize`, saturating, for sizing buffers.
fn count(frames: Frames) -> usize {
    usize::try_from(frames.0).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Codec,
        replay::{Matrix, Packet, Picture, Stream},
    };
    use std::cell::RefCell;

    const TEN_MS: i64 = 100_000;
    const STEREO_16: Layout = Layout {
        sample: pcm::Sample::Int16,
        channels: 2,
    };

    /// The runs a track handed over, as (first frame, samples).
    #[derive(Default)]
    struct Runs(RefCell<Vec<(u64, Vec<i16>)>>);

    impl Sink for Runs {
        fn push_audio(&self, start: u64, data: Arc<[i16]>) {
            self.0.borrow_mut().push((start, data.to_vec()));
        }
    }

    impl Runs {
        /// Each run's first frame and length in frames.
        fn spans(&self) -> Vec<(u64, usize)> {
            self.0
                .borrow()
                .iter()
                .map(|(start, samples)| (*start, samples.len() / 2))
                .collect()
        }
    }

    /// Stereo 16-bit frames numbered `first..first + frames`, both channels holding the number.
    fn numbered(first: u16, frames: u16) -> Vec<u8> {
        (first..first + frames)
            .flat_map(|number| {
                let [low, high] = number.to_le_bytes();
                [low, high, low, high]
            })
            .collect()
    }

    fn left(samples: &[i16]) -> Vec<i16> {
        samples.chunks(2).map(|frame| frame[0]).collect()
    }

    fn feed(track: &mut Track<'_>, time: i64, frames: u32, data: Option<&[u8]>) {
        track.packet(
            &loopback::Packet {
                data,
                frames,
                time: Some(Ticks(time)),
            },
            STEREO_16,
        );
    }

    #[test]
    fn packets_on_time_are_handed_over_as_one_run() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(0, 480)));
        feed(&mut track, TEN_MS, 480, Some(&numbered(480, 480)));
        assert!(runs.0.borrow().is_empty());
        track.flush();
        assert_eq!(runs.spans(), [(0, 960)]);
        let expected: Vec<i16> = (0..960).collect();
        assert_eq!(left(&runs.0.borrow()[0].1), expected);
    }

    #[test]
    fn a_gap_in_the_stamps_is_left_between_two_runs() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(0, 480)));
        feed(&mut track, 5 * TEN_MS, 480, Some(&numbered(480, 480)));
        track.flush();
        assert_eq!(runs.spans(), [(0, 480), (2400, 480)]);
    }

    #[test]
    fn a_silent_packet_is_left_as_a_gap() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(0, 480)));
        feed(&mut track, TEN_MS, 480, None);
        feed(&mut track, 2 * TEN_MS, 480, Some(&numbered(960, 480)));
        track.flush();
        assert_eq!(runs.spans(), [(0, 480), (960, 480)]);
        assert_eq!(runs.0.borrow()[1].1[0], 960);
    }

    #[test]
    fn frames_overlapping_ones_placed_are_dropped_from_the_front() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 4800, Some(&numbered(0, 4800)));
        feed(
            &mut track,
            10 * TEN_MS - TEN_MS / 2,
            480,
            Some(&numbered(10_000, 480)),
        );
        track.flush();
        assert_eq!(runs.spans(), [(0, 4800 + 240)]);
        let samples = left(&runs.0.borrow()[0].1);
        assert_eq!(samples[4799], 4799);
        assert_eq!(samples[4800], 10_240);
        assert_eq!(samples[4800 + 239], 10_479);
    }

    #[test]
    fn a_packet_short_of_data_is_padded_to_the_frames_it_claims() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(1, 100)));
        feed(&mut track, TEN_MS, 480, Some(&numbered(1, 480)));
        track.flush();
        assert_eq!(runs.spans(), [(0, 960)]);
        let samples = left(&runs.0.borrow()[0].1);
        assert_eq!(samples[99], 100);
        assert!(samples[100..480].iter().all(|&sample| sample == 0));
        assert_eq!(samples[480], 1);
    }

    #[test]
    fn catching_up_hands_over_the_run_and_leaves_silence_behind_it() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(0, 480)));
        track.catch_up(Ticks(TEN_MS / 2));
        assert!(runs.0.borrow().is_empty(), "nothing is missing yet");
        track.catch_up(Ticks(5 * TEN_MS));
        assert_eq!(runs.spans(), [(0, 480)]);
        feed(&mut track, 5 * TEN_MS, 480, Some(&numbered(0, 480)));
        track.flush();
        assert_eq!(runs.spans(), [(0, 480), (2400, 480)]);
    }

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
        live.push(packet(0), Picture::Fresh);
        let capture = start(&live).unwrap();
        thread::sleep(Duration::from_secs(10));
        live.push(packet(599), Picture::Fresh);
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
        assert!((469..=471).contains(&audio.frames.len()));
        if let Some(path) = std::env::var_os("HINDSIGHT_AUDIO_OUTPUT") {
            std::fs::write(path, crate::audio::testing::adts(&audio.frames)).unwrap();
        }
    }
}
