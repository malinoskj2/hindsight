//! The session's audio thread: reads the loopback capture on a timer, keeps the sound on the
//! shared clock and hands it to the buffer as 16-bit PCM; gaps are left for a save to fill.

use super::{
    CHANNELS, SAMPLE_RATE, aac,
    loopback::{self, Devices, Loopback, OpenError, Packet, Signal, Wake, Watch},
    pcm::{self, Layout},
    timeline::Timeline,
};
use crate::{
    log,
    replay::{AudioFormat, CLOCK_RATE, Live},
    sys::{self, clock, com::Apartment},
};
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{Foundation::HANDLE, Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED};

/// Six 10 ms engine packets per read; the 200 ms capture buffer covers a late one.
const READ_INTERVAL: Duration = Duration::from_millis(60);
const WAKE_INTERVAL: Duration = Duration::from_millis(100);
/// Silence is filled in this far behind the present, so a late packet is not taken for a gap.
const SILENCE_LAG: i64 = CLOCK_RATE / 2;
const REOPEN_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

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

fn record(live: &Live, stop: &Signal) {
    let Some(mut pipeline) = Pipeline::open(live) else {
        return;
    };
    let mut device = Device::new(watch_default(&pipeline.devices));
    let mut placed = false;
    loop {
        if !placed && let Some(capture) = live.capture_processor() {
            sys::thread::keep_off(capture);
            placed = true;
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

/// Fields drop in this order: the device list releases its COM objects before COM shuts down.
struct Pipeline<'a> {
    track: Track<'a>,
    devices: Devices,
    _com: Apartment,
}

impl<'a> Pipeline<'a> {
    fn open(live: &'a Live) -> Option<Self> {
        let com = started(Apartment::multi_threaded(), "COM is unavailable")?;
        let devices = started(Devices::new(), "the audio devices could not be listed")?;
        Some(Self {
            track: Track::open(live),
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
    loopback: Option<Loopback>,
    /// `None` means the default device is polled.
    watch: Option<Watch>,
    due: Option<Instant>,
    opened_before: bool,
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

    fn interval(&self) -> Duration {
        if self.loopback.is_some() {
            READ_INTERVAL
        } else {
            WAKE_INTERVAL
        }
    }

    fn wake(&self) -> Option<HANDLE> {
        self.watch.as_ref().map(Watch::event)
    }

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

    fn switch(&mut self, devices: &Devices, now: Instant) {
        if self.loopback.take().is_some() {
            log::info("The default audio output changed; switching to it");
        }
        self.open(devices, now);
    }

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

    fn lost(&mut self, error: &windows::core::Error) {
        if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            log::info("The audio output device went away; reopening");
        } else {
            log::warning(format!("Sound capture failed ({error}); reopening"));
        }
        self.loopback = None;
        self.due = None;
    }

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

struct Track<'a> {
    live: &'a Live,
    timeline: Timeline,
    /// Number of the next sample, silence included.
    next: u64,
    pending: Vec<i16>,
    pending_start: u64,
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
        Self {
            live,
            timeline: Timeline::new(SAMPLE_RATE, anchor),
            next: 0,
            pending: Vec::new(),
            pending_start: 0,
        }
    }

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
        // Pad to the frames the packet claims, whatever its data holds.
        self.pending.resize(length, 0);
        self.next += u64::from(kept);
    }

    fn catch_up(&mut self, time: i64) {
        let silence = self.timeline.catch_up(time);
        self.skip(silence);
    }

    fn skip(&mut self, samples: u64) {
        if samples > 0 {
            self.flush();
            self.next += samples;
        }
    }

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
        replay::{Matrix, Packet, Picture, Stream},
    };

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
