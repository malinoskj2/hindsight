//! The audio thread of a session: it opens the default output device's loopback capture and
//! the AAC encoder, then every few packets reads, places and encodes what has arrived, until
//! the session stops it. It keeps the sound unbroken on the shared clock through everything a
//! device does: silence fills a gap, a quiet device (Windows sends a loopback capture nothing
//! while nothing plays), a lost device and the time a reopen takes; a packet marked as a
//! discontinuity is simply stamped later than the run expects, which reads as a gap.

use super::{
    CHANNELS, SAMPLE_RATE,
    aac::{self, Encoder, MediaFoundation},
    loopback::{self, Com, Devices, Loopback, OpenError, Packet, Signal, Wake, Watch},
    pcm::{self, Layout},
    timeline::Timeline,
};
use crate::{
    log,
    replay::{AudioFormat, AudioFrame, CLOCK_RATE, FRAME_SAMPLES, Live},
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
/// Silence encoded in a row before more is skipped rather than encoded: enough to push out
/// everything the encoder holds back and its priming, with a frame to spare, so every frame it
/// still holds is silent too and the skipped ones can be left out of the buffer, where a save
/// stands in a silent frame.
const FLUSH_SAMPLES: u64 = (aac::HELD_BACK + aac::PRIMING + FRAME_SAMPLES) as u64;

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

/// Why the thread stopped reading.
enum Failure {
    /// The device failed; it is opened again.
    Device(windows::core::Error),
    /// The encoder failed; the session's sound ends there.
    Encoder(windows::core::Error),
}

impl From<windows::core::Error> for Failure {
    fn from(error: windows::core::Error) -> Self {
        Self::Device(error)
    }
}

/// Records sound into `live` until `stop` is set.
fn record(live: &Live, stop: &Signal) {
    let started = Com::start().and_then(|com| Ok((com, MediaFoundation::start()?)));
    let (_com, _media) = match started {
        Ok(started) => started,
        Err(error) => {
            log::warning(format!(
                "Sound is not recorded: Media Foundation is unavailable ({error})"
            ));
            return;
        }
    };
    let opened = Devices::new().and_then(|devices| Ok((devices, Track::open(live)?)));
    let (devices, mut track) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            log::warning(format!(
                "Sound is not recorded: the AAC encoder could not be opened ({error})"
            ));
            return;
        }
    };

    let watch = match devices.watch() {
        Ok(watch) => Some(watch),
        Err(error) => {
            log::warning(format!(
                "Windows will not report audio device changes ({error}); checking every {} s instead",
                DEFAULT_CHECK_INTERVAL.as_secs()
            ));
            None
        }
    };
    let mut device = Device::new(watch);
    let mut placed = false;
    loop {
        if !placed {
            placed = sys::thread::keep_off_capture_core();
        }
        device.update(&devices);
        if let Wake::Stop = loopback::wait(stop, device.wake(), device.interval()) {
            break;
        }
        let result = device
            .read(&mut track)
            .and_then(|()| track.catch_up(clock::now() - SILENCE_LAG));
        match result {
            Ok(()) => {}
            Err(Failure::Device(error)) => device.lost(&error),
            Err(Failure::Encoder(error)) => {
                log::error(format!(
                    "Sound recording stopped: the AAC encoder failed ({error})"
                ));
                return;
            }
        }
    }
    log::info("Sound recording stopped");
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
    fn read(&mut self, track: &mut Track<'_>) -> Result<(), Failure> {
        let Some(loopback) = self.loopback.as_mut() else {
            return Ok(());
        };
        let layout = loopback.layout();
        loopback.read(|packet| track.packet(packet, layout))
    }
}

/// The session's run of sound: the encoder, where the run stands on the clock, and the number
/// of the next frame.
struct Track<'a> {
    live: &'a Live,
    encoder: Encoder,
    timeline: Timeline,
    /// The number the encoder's next frame gets.
    next_frame: u64,
    /// Samples of silence encoded since the last captured sound.
    quiet: u64,
    /// A packet converted for the encoder, kept to reuse its allocation.
    pcm: Vec<i16>,
    /// A frame of silence for the encoder.
    zeros: Vec<i16>,
}

impl<'a> Track<'a> {
    /// Opens the encoder and records the stream's format in `live`, with the run starting now.
    fn open(live: &'a Live) -> windows::core::Result<Self> {
        let encoder = Encoder::open(SAMPLE_RATE, CHANNELS)?;
        let silence = aac::silent_frame(SAMPLE_RATE, CHANNELS)?;
        let anchor = clock::now();
        live.set_audio(AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            config: encoder.config(),
            anchor,
            priming: aac::PRIMING,
            silence,
        });
        Ok(Self {
            live,
            encoder,
            timeline: Timeline::new(SAMPLE_RATE, anchor),
            next_frame: 0,
            quiet: 0,
            pcm: Vec::new(),
            zeros: vec![0; FRAME_SAMPLES as usize * usize::from(CHANNELS)],
        })
    }

    /// Places `packet`, whose samples are laid out as `layout` says, on the run and encodes it.
    fn packet(&mut self, packet: &Packet<'_>, layout: Layout) -> Result<(), Failure> {
        let placement = self.timeline.place(packet.time, packet.frames);
        self.silence(placement.silence)?;
        let kept = packet.frames - placement.skip;
        let Some(data) = packet.data else {
            return self.silence(u64::from(kept));
        };
        self.pcm.clear();
        let skipped_bytes = placement.skip as usize * layout.frame_bytes();
        pcm::append_stereo(
            &mut self.pcm,
            &data[skipped_bytes.min(data.len())..],
            layout,
        );
        self.quiet = 0;
        encode(
            &mut self.encoder,
            self.live,
            &mut self.next_frame,
            &self.pcm,
        )
    }

    /// Fills the run with silence up to clock time `time`, if nothing has arrived to fill it.
    fn catch_up(&mut self, time: i64) -> Result<(), Failure> {
        let silence = self.timeline.catch_up(time);
        self.silence(silence)
    }

    /// Adds `samples` of silence to the run. Once the encoder has been given enough silence to
    /// hold nothing else, whole frames of more are skipped: their numbers are passed over and
    /// a save fills them with the silent frame.
    fn silence(&mut self, samples: u64) -> Result<(), Failure> {
        let frame = u64::from(FRAME_SAMPLES);
        let flushing = samples.min(FLUSH_SAMPLES.saturating_sub(self.quiet));
        let rest = samples - flushing;
        let mut encoded = flushing + rest % frame;
        self.quiet = self.quiet.saturating_add(encoded);
        while encoded > 0 {
            let now = encoded.min(frame);
            let length = usize::try_from(now).unwrap_or(0) * usize::from(CHANNELS);
            encode(
                &mut self.encoder,
                self.live,
                &mut self.next_frame,
                &self.zeros[..length],
            )?;
            encoded -= now;
        }
        self.next_frame += rest / frame;
        Ok(())
    }
}

/// Encodes `pcm` and adds each finished frame to `live` under the next number.
fn encode(
    encoder: &mut Encoder,
    live: &Live,
    next_frame: &mut u64,
    pcm: &[i16],
) -> Result<(), Failure> {
    encoder
        .encode(pcm, |data| {
            live.push_audio(AudioFrame {
                number: *next_frame,
                data,
            });
            *next_frame += 1;
        })
        .map_err(Failure::Encoder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Codec,
        replay::{Packet, Stream},
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
        let audio = live.clip().unwrap().audio.unwrap();
        let silent = audio
            .frames
            .iter()
            .filter(|frame| Arc::ptr_eq(frame, &audio.format.silence))
            .count();
        println!(
            "{} frames for 10 s of video, {silent} of them stand-ins for silence, lead {}",
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
