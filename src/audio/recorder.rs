//! The session's audio thread. It reads each source's capture on a timer, keeps the sound on the
//! shared clock and passes it to the buffer as 16-bit PCM. Gaps are left for a save to fill.

use super::{
    SAMPLE_RATE, Sources, aac,
    pcm::{Channels, Layout},
    timeline::{Frames, Ticks, Timeline},
    wasapi::{self, Devices, OpenError, Packet, Signal, Wake, Watch},
};
use crate::{
    log,
    replay::{AudioFormat, CLOCK_RATE, Live, Source, TrackFormat},
    sys::{self, clock, com::Apartment},
};
use std::{
    any::Any,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED;

/// Each read takes six 10 ms engine packets. The 200 ms capture buffer absorbs a late read.
const READ_INTERVAL: Duration = Duration::from_millis(60);
const WAKE_INTERVAL: Duration = Duration::from_millis(100);
/// Silence is filled in this far behind the present, so that a late packet is not mistaken for a
/// gap.
const SILENCE_LAG: Ticks = Ticks(CLOCK_RATE / 2);
const REOPEN_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct Capture {
    stop: Arc<Signal>,
    join: Option<JoinHandle<()>>,
}

impl Capture {
    /// Returns whether the audio thread ended by itself, so that sound is no longer recorded.
    pub(crate) fn has_stopped(&self) -> bool {
        self.join.as_ref().is_some_and(JoinHandle::is_finished)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(join) = self.join.take()
            && let Err(payload) = join.join()
        {
            log::error(format!(
                "Sound · audio thread panicked · {}",
                panic_message(&*payload)
            ));
        }
    }
}

/// Returns the text a panic was raised with, when the payload is a string.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

pub(crate) fn start(live: &Arc<Live>, sources: Sources) -> Option<Capture> {
    let stop = match Signal::latch() {
        Ok(signal) => Arc::new(signal),
        Err(error) => {
            log::warning(format!("Sound · not recorded ({error})"));
            return None;
        }
    };
    let (live, signal) = (Arc::clone(live), Arc::clone(&stop));
    match thread::Builder::new()
        .name("audio".to_owned())
        .spawn(move || record(&live, sources, &signal))
    {
        Ok(join) => Some(Capture {
            stop,
            join: Some(join),
        }),
        Err(error) => {
            log::warning(format!(
                "Sound · not recorded · audio thread did not start ({error})"
            ));
            None
        }
    }
}

/// The channels each source is kept and encoded in. The microphone is kept in mono, which halves
/// its memory.
const fn kept_in(source: Source) -> Channels {
    match source {
        Source::System => Channels::Stereo,
        Source::Microphone => Channels::Mono,
    }
}

/// The name log lines give a source, and the name of its device.
const fn names(source: Source) -> (&'static str, &'static str) {
    match source {
        Source::System => ("Sound", "output device"),
        Source::Microphone => ("Microphone", "microphone"),
    }
}

fn record(live: &Live, sources: Sources, stop: &Signal) {
    let Some(mut pipeline) = Pipeline::open(live, sources) else {
        return;
    };
    let Pipeline {
        inputs,
        watch,
        devices,
        ..
    } = &mut pipeline;
    let unchanged = if watch.is_some() {
        Defaults::Unchanged
    } else {
        Defaults::Unwatched
    };
    let mut placed = false;
    let mut defaults = unchanged;
    loop {
        if !placed && let Some(capture) = live.capture_processor() {
            sys::thread::keep_off(capture);
            placed = true;
        }
        for input in inputs.iter_mut() {
            input.device.update(devices, defaults);
        }
        let interval = inputs
            .iter()
            .map(|input| input.device.interval())
            .min()
            .unwrap_or(WAKE_INTERVAL);
        // A failed wait would fail again at once, so stopping is preferable to spinning on it.
        let wake = match wasapi::wait(stop, watch.as_ref().map(Watch::event), interval) {
            Ok(Wake::Stop) => break,
            Ok(wake) => wake,
            Err(error) => {
                log::error(format!("Sound · audio thread could not wait ({error})"));
                break;
            }
        };
        defaults = match wake {
            Wake::Changed => Defaults::Changed,
            Wake::Stop | Wake::Read => unchanged,
        };
        let settled = Ticks(clock::now()).saturating_sub(SILENCE_LAG);
        for input in inputs.iter_mut() {
            input.read(settled);
        }
    }
    log::info("Sound · stopped");
}

/// Fields drop in this order: the open streams, the watch and the device list release their COM
/// objects before COM shuts down.
struct Pipeline<'a> {
    inputs: Vec<Input<'a>>,
    watch: Option<Watch>,
    devices: Devices,
    _com: Apartment,
}

impl<'a> Pipeline<'a> {
    fn open(live: &'a Live, sources: Sources) -> Option<Self> {
        let com = started(Apartment::multi_threaded(), "COM unavailable")?;
        let devices = started(Devices::new(), "devices not listed")?;
        let anchor = clock::now();
        live.set_audio(AudioFormat {
            sample_rate: SAMPLE_RATE,
            tracks: sources
                .iter()
                .map(|source| TrackFormat {
                    source,
                    channels: kept_in(source).count(),
                })
                .collect(),
            anchor,
            priming: aac::PRIMING,
        });
        let inputs = sources
            .iter()
            .map(|source| Input {
                track: Track::new(live, source, Ticks(anchor)),
                device: Device::new(source),
            })
            .collect();
        Some(Self {
            inputs,
            watch: watch_defaults(&devices),
            devices,
            _com: com,
        })
    }
}

fn started<T>(result: windows::core::Result<T>, what: &str) -> Option<T> {
    result
        .inspect_err(|error| log::warning(format!("Sound · not recorded · {what} ({error})")))
        .ok()
}

fn watch_defaults(devices: &Devices) -> Option<Watch> {
    devices
        .watch()
        .inspect_err(|error| {
            log::warning(format!(
                "Sound · no device change events ({error}) · checking every {} s",
                DEFAULT_CHECK_INTERVAL.as_secs()
            ));
        })
        .ok()
}

/// What is known of the default devices since the last update.
#[derive(Clone, Copy)]
enum Defaults {
    /// The watch reported that a default device changed, perhaps another source's.
    Changed,
    Unchanged,
    /// Nothing reports changes, so the defaults are checked on a timer.
    Unwatched,
}

/// One source: its device and the track its sound is kept on.
struct Input<'a> {
    track: Track<'a>,
    device: Device,
}

impl Input<'_> {
    /// Reads what the device captured, and passes the track on up to `settled`, before which
    /// whatever was not captured is silence.
    fn read(&mut self, settled: Ticks) {
        if let Err(error) = self.device.read(&mut self.track) {
            self.device.lost(&error);
        }
        self.track.catch_up(settled);
        self.track.flush();
    }
}

struct Device {
    source: Source,
    state: DeviceState,
    /// When to next check the default device or try to open one. `None` means now.
    due: Option<Instant>,
}

enum DeviceState {
    Open(wasapi::Stream),
    Closed {
        /// A device was open before, so opening one is logged as a return.
        opened_before: bool,
        /// A failure to open has been logged since the last success, so another is not.
        reported: bool,
    },
}

impl DeviceState {
    /// The state after a device that was open closes.
    const CLOSED_AFTER_OPEN: Self = Self::Closed {
        opened_before: true,
        reported: false,
    };
}

impl Device {
    const fn new(source: Source) -> Self {
        Self {
            source,
            state: DeviceState::Closed {
                opened_before: false,
                reported: false,
            },
            due: None,
        }
    }

    const fn interval(&self) -> Duration {
        match self.state {
            DeviceState::Open(_) => READ_INTERVAL,
            DeviceState::Closed { .. } => WAKE_INTERVAL,
        }
    }

    fn update(&mut self, devices: &Devices, defaults: Defaults) {
        let now = Instant::now();
        if let Defaults::Changed = defaults {
            self.follow_default(devices, now);
            return;
        }
        if self.due.is_some_and(|due| now < due) {
            return;
        }
        match (&self.state, defaults) {
            (DeviceState::Closed { .. }, _) => self.open(devices, now),
            (DeviceState::Open(_), Defaults::Unwatched) => self.follow_default(devices, now),
            (DeviceState::Open(_), Defaults::Changed | Defaults::Unchanged) => {
                self.due = Some(now + DEFAULT_CHECK_INTERVAL);
            }
        }
    }

    /// Switches to the default device if it is no longer the one open.
    fn follow_default(&mut self, devices: &Devices, now: Instant) {
        let DeviceState::Open(stream) = &self.state else {
            self.open(devices, now);
            return;
        };
        if devices
            .default_id(self.source)
            .is_some_and(|id| id != stream.id())
        {
            let (label, device) = names(self.source);
            log::info(format!("{label} · default {device} changed · switching"));
            self.state = DeviceState::CLOSED_AFTER_OPEN;
            self.open(devices, now);
            return;
        }
        self.due = Some(now + DEFAULT_CHECK_INTERVAL);
    }

    fn open(&mut self, devices: &Devices, now: Instant) {
        let DeviceState::Closed {
            opened_before,
            reported,
        } = &mut self.state
        else {
            return;
        };
        let (label, device) = names(self.source);
        match devices.open(self.source) {
            Ok(stream) => {
                log::info(format!(
                    "{label} · {}{}",
                    stream.name(),
                    if *opened_before { " · again" } else { "" }
                ));
                self.state = DeviceState::Open(stream);
                self.due = Some(now + DEFAULT_CHECK_INTERVAL);
            }
            Err(error) => {
                if !*reported {
                    match &error {
                        OpenError::NoDevice => {
                            log::info(format!("{label} · no {device} · silence until one appears"));
                        }
                        OpenError::Windows(_, cause) => log::warning(format!(
                            "{label} · cannot capture ({error}: {cause}) · retrying every second"
                        )),
                    }
                    *reported = true;
                }
                self.due = Some(now + REOPEN_INTERVAL);
            }
        }
    }

    fn lost(&mut self, error: &windows::core::Error) {
        let (label, device) = names(self.source);
        if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            log::info(format!("{label} · {device} went away · reopening"));
        } else {
            log::warning(format!("{label} · capture failed ({error}) · reopening"));
        }
        self.state = DeviceState::CLOSED_AFTER_OPEN;
        self.due = None;
    }

    fn read(&mut self, track: &mut Track<'_>) -> windows::core::Result<()> {
        let DeviceState::Open(stream) = &mut self.state else {
            return Ok(());
        };
        let layout = stream.layout();
        stream.read(|packet| {
            track.packet(packet, layout);
            Ok(())
        })
    }
}

/// The destination the track passes finished runs of sound to: the live buffer, or a test's list.
trait Sink {
    /// Interleaved samples of `source` starting at frame `start`.
    fn push_audio(&self, source: Source, start: u64, data: Arc<[i16]>);
}

impl Sink for Live {
    fn push_audio(&self, source: Source, start: u64, data: Arc<[i16]>) {
        Live::push_audio(self, source, start, data);
    }
}

struct Track<'a> {
    sink: &'a dyn Sink,
    source: Source,
    timeline: Timeline,
    /// Number of the next frame, silence included.
    next: Frames,
    /// Interleaved samples, in the channels the source is kept in, not yet passed to the buffer.
    pending: Vec<i16>,
    /// Number of the first frame in `pending`.
    pending_start: Frames,
}

impl<'a> Track<'a> {
    fn new(sink: &'a dyn Sink, source: Source, anchor: Ticks) -> Self {
        Self {
            sink,
            source,
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
        let channels = kept_in(self.source);
        let length =
            self.pending.len() + count(kept).saturating_mul(usize::from(channels.count().get()));
        let dropped_bytes = count(placement.overlap).saturating_mul(layout.frame_bytes());
        channels.append(
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
            self.sink.push_audio(
                self.source,
                self.pending_start.0,
                Arc::from(&self.pending[..]),
            );
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
    use super::{super::pcm, *};
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

    /// The runs a track passed on, as (first frame, samples).
    #[derive(Default)]
    struct Runs(RefCell<Vec<(u64, Vec<i16>)>>);

    impl Sink for Runs {
        fn push_audio(&self, _source: Source, start: u64, data: Arc<[i16]>) {
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
            &wasapi::Packet {
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
        let mut track = Track::new(&runs, Source::System, Ticks(0));
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
        let mut track = Track::new(&runs, Source::System, Ticks(0));
        feed(&mut track, 0, 480, Some(&numbered(0, 480)));
        feed(&mut track, 5 * TEN_MS, 480, Some(&numbered(480, 480)));
        track.flush();
        assert_eq!(runs.spans(), [(0, 480), (2400, 480)]);
    }

    #[test]
    fn a_silent_packet_is_left_as_a_gap() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Source::System, Ticks(0));
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
        let mut track = Track::new(&runs, Source::System, Ticks(0));
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
        let mut track = Track::new(&runs, Source::System, Ticks(0));
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
        let mut track = Track::new(&runs, Source::System, Ticks(0));
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
    fn the_microphone_track_keeps_one_channel() {
        let runs = Runs::default();
        let mut track = Track::new(&runs, Source::Microphone, Ticks(0));
        let stereo: Vec<u8> = [100_i16, 300, -100, -300]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        feed(&mut track, 0, 480, Some(&stereo));
        feed(&mut track, TEN_MS, 480, Some(&numbered(0, 100)));
        track.flush();
        let runs = runs.0.borrow();
        let [(start, samples)] = &runs[..] else {
            panic!("expected one run");
        };
        assert_eq!(*start, 0);
        assert_eq!(samples.len(), 960);
        assert_eq!(samples[..2], [200, -200]);
        assert!(samples[2..480].iter().all(|&sample| sample == 0));
        assert_eq!(samples[480 + 99], 99);
        assert!(samples[480 + 100..].iter().all(|&sample| sample == 0));
    }

    #[test]
    #[ignore = "records the machine's sound and microphone for ten seconds"]
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
        let sources = Sources {
            system: true,
            microphone: true,
        };
        let capture = start(&live, sources).unwrap();
        thread::sleep(Duration::from_secs(10));
        live.push(packet(599), Picture::Fresh);
        drop(capture);

        for entry in log::since(0) {
            println!("{}", entry.line());
        }
        let tracks = crate::audio::encode(&live.clip().unwrap().audio.unwrap()).unwrap();
        let [system, microphone] = &tracks[..] else {
            panic!("expected two tracks");
        };
        for audio in [system, microphone] {
            println!(
                "{:?}: {} frames for 10 s of video, lead {}",
                audio.source,
                audio.frames.len(),
                audio.lead
            );
            assert!((469..=471).contains(&audio.frames.len()));
        }
        if let Some(path) = std::env::var_os("HINDSIGHT_AUDIO_OUTPUT") {
            std::fs::write(path, crate::audio::testing::adts(&system.frames)).unwrap();
        }
    }
}
