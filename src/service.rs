//! The recording worker: keeps one capture session running and saves clips from its buffer.

use crate::{
    audio,
    config::{Backend, Config, Memory, Mode, Theme},
    log,
    replay::{self, Held, Live, SaveError, Snapshot, Stream},
    storage::{StorageError, Store, Totals},
    sys::{self, sound::Tone},
};
use anyhow::Result;
use std::{
    fmt::Display,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) mod text;

const RATE_SPAN: Duration = Duration::from_secs(10);
const FIRST_RATE_SPAN: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Handles are the OS's, carried as integers so the worker stays portable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Handle 0 is the primary monitor.
    Monitor { handle: isize, name: String },
    Window {
        window: isize,
        monitor: isize,
        name: String,
    },
}

impl Target {
    pub(crate) fn primary_monitor() -> Self {
        Self::Monitor {
            handle: 0,
            name: "the primary monitor".to_owned(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Monitor { name, .. } | Self::Window { name, .. } => name,
        }
    }

    fn monitor(&self) -> Self {
        match self {
            Self::Monitor { .. } => self.clone(),
            Self::Window { monitor, name, .. } => Self::Monitor {
                handle: *monitor,
                name: name.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Stopped,
    Resized,
    Closed,
    /// The app presents in a way Windows cannot compose; its monitor must be captured instead.
    NoFrames,
    /// Trying again would fail the same way.
    Unsupported(String),
}

type Recorder = fn(&Config, &Target, &Live, &AtomicBool) -> Result<Ended>;
type Audio = fn(&Arc<Live>) -> Option<audio::Capture>;
type Writer = fn(&Snapshot, &Path) -> Result<PathBuf, SaveError>;
type Chime = Box<dyn Fn(Tone) + Send>;

#[cfg(windows)]
const RECORDER: Recorder = crate::gpu::record;
#[cfg(not(windows))]
const RECORDER: Recorder = |_, _, _, _| anyhow::bail!("capture requires Windows");

/// What the worker drives; tests stand in for any part of it.
struct Services {
    record: Recorder,
    audio: Audio,
    write: Writer,
    chime: Chime,
    retry_interval: Duration,
}

impl Services {
    fn real() -> Self {
        let sounds = sys::sound::start();
        Self {
            record: RECORDER,
            audio: audio::start,
            write: replay::save,
            chime: Box::new(move |tone| {
                let _ = sounds.send(tone);
            }),
            retry_interval: RETRY_INTERVAL,
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) enum DisplayStatus {
    #[default]
    Starting,
    Waiting,
    Recording,
    /// Capture stopped on request; what was buffered stays saveable until it resumes.
    Paused,
    Retrying,
    Unsupported,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Status {
    pub(crate) state: DisplayStatus,
    pub(crate) recording: bool,
    pub(crate) source: Option<String>,
    pub(crate) backend: Option<Backend>,
    pub(crate) stream: Option<Stream>,
    pub(crate) bitrate_mbps: u32,
    /// What the buffer holds now, and where it is kept while a session has one.
    pub(crate) buffer_held: Held,
    pub(crate) buffer_memory: Option<Memory>,
    pub(crate) buffer_seconds: u32,
    pub(crate) buffer_capacity_bytes: u64,
    pub(crate) saved_bytes: Option<u64>,
    pub(crate) buffered_lifetime_bytes: Option<u64>,
    pub(crate) encode_rate: Option<f64>,
    pub(crate) session_written_bytes: u64,
    pub(crate) session_saved_bytes: u64,
    pub(crate) last_clip: Option<PathBuf>,
    pub(crate) error: Option<String>,
    /// A save was asked for before the buffer held anything.
    pub(crate) nothing_to_save: bool,
}

#[derive(Debug)]
enum Command {
    Start(Config),
    Target(Target),
    Save,
    Pause,
    Resume,
    Restart,
    Theme(Theme),
    Wake,
    Exit,
}

pub(crate) struct Worker {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn spawn(store: Store, config: Config) -> io::Result<Self> {
        Self::spawn_with(Services::real(), Some(store), config)
    }

    fn spawn_with(services: Services, store: Option<Store>, config: Config) -> io::Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let wake = commands.clone();
        let status = Arc::new(Mutex::new(Status::default()));
        let output = Arc::clone(&status);
        let join = thread::Builder::new()
            .name("recording-worker".to_owned())
            .spawn(move || {
                let mut machine = Machine::new(services, store, config, output, wake);
                machine.run(&receiver);
            })?;

        Ok(Self {
            commands,
            status,
            join: Some(join),
        })
    }

    pub(crate) fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn start(&self, config: Config) {
        self.request(Command::Start(config));
    }

    pub(crate) fn target(&self, target: Target) {
        self.request(Command::Target(target));
    }

    pub(crate) fn save(&self) {
        self.request(Command::Save);
    }

    pub(crate) fn pause(&self) {
        self.request(Command::Pause);
    }

    pub(crate) fn resume(&self) {
        self.request(Command::Resume);
    }

    pub(crate) fn restart(&self) {
        self.request(Command::Restart);
    }

    pub(crate) fn set_theme(&self, theme: Theme) {
        self.request(Command::Theme(theme));
    }

    pub(crate) fn refresh(&self) {
        self.request(Command::Wake);
    }

    fn request(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.request(Command::Exit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Session {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    join: JoinHandle<Result<Ended>>,
    _audio: Option<audio::Capture>,
}

struct Saving {
    seconds: f64,
    done: Arc<AtomicBool>,
    join: JoinHandle<Result<PathBuf, SaveError>>,
    next_tick: Instant,
}

/// Dropped as a capture or save thread's closure ends: marks it done and wakes the worker.
/// `JoinHandle::is_finished` would not do, as it says no until the result is stored.
struct Ending {
    done: Arc<AtomicBool>,
    wake: Sender<Command>,
}

impl Drop for Ending {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let _ = self.wake.send(Command::Wake);
    }
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    encoded: u64,
}

struct Reading {
    running: bool,
    stream: Option<Stream>,
    live_bytes: u64,
    held: Option<Held>,
    sample: Sample,
}

/// The bytes buffered and saved: this run's, and the lifetime totals kept in the store.
struct Ledger {
    store: Option<Store>,
    totals: Option<Totals>,
    session_buffered: u64,
    session_saved: u64,
}

impl Ledger {
    fn new(store: Option<Store>) -> Self {
        let totals = store.as_ref().and_then(load_totals);
        Self {
            store,
            totals,
            session_buffered: 0,
            session_saved: 0,
        }
    }

    fn totals(&self) -> Option<Totals> {
        self.totals
    }

    fn save_config(&self, config: &Config) -> Result<(), StorageError> {
        self.store
            .as_ref()
            .map_or(Ok(()), |store| store.save_config(config))
    }

    /// Counts a replay buffer that is done with.
    fn bank(&mut self, live: &Live) {
        let bytes = live.bytes();
        self.session_buffered = self.session_buffered.saturating_add(bytes);
        self.add(bytes, |totals| &mut totals.buffered_bytes);
    }

    fn add_saved(&mut self, path: &Path) {
        let bytes = match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                log::warning(text::clip_size_unreadable(path, error));
                0
            }
        };
        self.session_saved = self.session_saved.saturating_add(bytes);
        self.add(bytes, |totals| &mut totals.saved_bytes);
    }

    fn add(&mut self, bytes: u64, total: fn(&mut Totals) -> &mut u64) {
        if let Some(totals) = self.totals.as_mut() {
            let total = total(totals);
            *total = total.saturating_add(bytes);
        }
        if let (Some(store), Some(totals)) = (&self.store, &self.totals)
            && let Err(error) = store.save_totals(totals)
        {
            log::warning(text::save_totals_failed(log::chain(&error)));
        }
    }
}

struct Machine {
    services: Services,
    ledger: Ledger,
    config: Config,
    output: Arc<Mutex<Status>>,
    status: Status,
    session: Option<Session>,
    target: Option<Target>,
    live: Option<Arc<Live>>,
    paused: bool,
    retry_at: Option<Instant>,
    saving: Option<Saving>,
    sample: Option<Sample>,
    wake: Sender<Command>,
}

impl Machine {
    fn new(
        services: Services,
        store: Option<Store>,
        config: Config,
        output: Arc<Mutex<Status>>,
        wake: Sender<Command>,
    ) -> Self {
        Self {
            services,
            ledger: Ledger::new(store),
            config,
            output,
            status: Status::default(),
            session: None,
            target: None,
            live: None,
            paused: false,
            retry_at: None,
            saving: None,
            sample: None,
            wake,
        }
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        self.begin();
        self.publish();
        loop {
            match self.receive(commands) {
                Ok(Command::Start(config)) => self.apply(config),
                Ok(Command::Target(target)) => self.offer(target),
                Ok(Command::Save) => self.save(),
                Ok(Command::Pause) => self.pause(),
                Ok(Command::Resume) => self.resume(),
                Ok(Command::Restart) => self.restart(),
                Ok(Command::Theme(theme)) => self.remember_theme(theme),
                Ok(Command::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Command::Exit) | Err(RecvTimeoutError::Disconnected) => break,
            }
            self.poll();
            self.publish();
        }
        self.shut_down();
    }

    fn receive(&self, commands: &Receiver<Command>) -> Result<Command, RecvTimeoutError> {
        let tick = self.saving.as_ref().map(|saving| saving.next_tick);
        match self.retry_at.into_iter().chain(tick).min() {
            Some(due) => commands.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }

    fn shut_down(&mut self) {
        self.stop_session();
        if let Some(saving) = self.saving.take() {
            self.finish_save(saving);
        }
        self.bank_live();
        self.publish();
    }

    fn apply(&mut self, config: Config) {
        if !self.save_settings(&config) {
            return;
        }
        log::info(text::settings_applied(&config));
        self.config = config;
        self.paused = false;
        self.stop_session();
        self.begin();
    }

    /// Stops capturing but keeps the buffer, so a clip can still be saved from before the pause.
    fn pause(&mut self) {
        if self.paused {
            return;
        }
        log::info(text::PAUSED);
        self.paused = true;
        self.retry_at = None;
        self.stop_session();
        self.status.state = DisplayStatus::Paused;
        self.status.error = None;
    }

    fn resume(&mut self) {
        if !self.paused {
            return;
        }
        log::info(text::RESUMED);
        self.paused = false;
        self.begin();
    }

    /// Tears the session down and builds it again with the same target.
    fn restart(&mut self) {
        log::info(text::RESTARTING);
        self.paused = false;
        self.stop_session();
        self.restart_session();
    }

    fn offer(&mut self, target: Target) {
        if self.config.mode != Mode::Fullscreen
            || self.paused
            || self.retry_at.is_some()
            || self.status.state == DisplayStatus::Unsupported
        {
            return;
        }
        let same = self.session.is_some() && self.target.as_ref() == Some(&target);
        if same {
            return;
        }
        self.stop_session();
        log::info(text::target_found(target.name()));
        self.start_session(target);
    }

    fn save(&mut self) {
        if self.saving.is_some() {
            log::info(text::ALREADY_SAVING);
            return;
        }
        let Some(snapshot) = self.live.as_ref().and_then(|live| live.snapshot()) else {
            log::warning(text::NOTHING_TO_SAVE);
            self.status.nothing_to_save = true;
            (self.services.chime)(Tone::Failed);
            return;
        };

        let seconds = snapshot.seconds();
        let (done, ending) = self.ending();
        match self.spawn_save(snapshot, ending) {
            Ok(join) => {
                (self.services.chime)(Tone::Progress);
                self.saving = Some(Saving {
                    seconds,
                    done,
                    join,
                    next_tick: Instant::now() + PROGRESS_INTERVAL,
                });
                self.status.nothing_to_save = false;
            }
            Err(error) => self.report_failed_save(text::save_failed(error)),
        }
    }

    fn remember_theme(&mut self, theme: Theme) {
        let config = Config {
            theme,
            ..self.config.clone()
        };
        self.save_settings(&config);
        self.config = config;
    }

    fn save_settings(&mut self, config: &Config) -> bool {
        match self.ledger.save_config(config) {
            Ok(()) => true,
            Err(error) => {
                self.report_error(text::save_settings_failed(log::chain(&error)));
                false
            }
        }
    }

    fn begin(&mut self) {
        self.retry_at = None;
        match self.config.mode {
            Mode::Desktop => self.start_session(Target::primary_monitor()),
            Mode::Fullscreen => {
                self.target = None;
                self.status.state = DisplayStatus::Waiting;
                self.status.error = None;
            }
        }
    }

    fn start_session(&mut self, target: Target) {
        self.bank_live();
        let live = Arc::new(Live::new(self.config.buffer_frames()));
        live.on_stream(self.waker());
        let stop = Arc::new(AtomicBool::new(false));
        let audio = self
            .config
            .audio
            .then(|| (self.services.audio)(&live))
            .flatten();
        let (done, ending) = self.ending();
        let spawned = self.spawn_capture(&target, &live, &stop, ending);

        self.retry_at = None;
        self.sample = None;
        self.target = Some(target);
        match spawned {
            Ok(join) => {
                self.session = Some(Session {
                    stop,
                    done,
                    join,
                    _audio: audio,
                });
                self.live = Some(live);
                self.status.state = DisplayStatus::Starting;
                self.status.error = None;
            }
            Err(error) => self.fail(text::capture_thread_failed(error)),
        }
    }

    fn spawn_capture(
        &self,
        target: &Target,
        live: &Arc<Live>,
        stop: &Arc<AtomicBool>,
        ending: Ending,
    ) -> io::Result<JoinHandle<Result<Ended>>> {
        let (record, config, target, live, stop) = (
            self.services.record,
            self.config.clone(),
            target.clone(),
            Arc::clone(live),
            Arc::clone(stop),
        );
        thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || {
                let _ending = ending;
                record(&config, &target, &live, &stop)
            })
    }

    fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let wake = self.wake.clone();
        move || {
            let _ = wake.send(Command::Wake);
        }
    }

    fn ending(&self) -> (Arc<AtomicBool>, Ending) {
        let done = Arc::new(AtomicBool::new(false));
        let ending = Ending {
            done: Arc::clone(&done),
            wake: self.wake.clone(),
        };
        (done, ending)
    }

    fn restart_session(&mut self) {
        match self.target.take() {
            Some(target) => self.start_session(target),
            None => self.begin(),
        }
    }

    fn stop_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        session.stop.store(true, Ordering::Relaxed);
        match session.join.join() {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => log::error(text::session_failed(format!("{error:#}"))),
            Err(_) => log::error(text::session_failed(text::CAPTURE_PANICKED)),
        }
    }

    fn fail(&mut self, error: impl Display) {
        self.report_error(text::session_failed(error));
        self.status.state = DisplayStatus::Retrying;
        self.retry_at = Some(Instant::now() + self.services.retry_interval);
    }

    fn report_error(&mut self, message: String) {
        log::error(&message);
        self.status.error = Some(message);
    }

    fn poll(&mut self) {
        if let Some(session) = self
            .session
            .take_if(|session| session.done.load(Ordering::Acquire))
        {
            self.session_ended(session.join.join());
        }

        if self.retry_at.is_some_and(|due| Instant::now() >= due) {
            self.retry();
        }

        // Before the tick, so no blip plays after the outcome.
        if let Some(saving) = self
            .saving
            .take_if(|saving| saving.done.load(Ordering::Acquire))
        {
            self.finish_save(saving);
        }
        self.tick_save();
    }

    fn session_ended(&mut self, outcome: thread::Result<Result<Ended>>) {
        match outcome {
            Ok(Ok(Ended::Stopped)) => {}
            Ok(Ok(Ended::Resized)) => {
                log::info(text::RESIZED);
                self.restart_session();
            }
            Ok(Ok(Ended::Closed)) if self.config.mode == Mode::Desktop => {
                self.fail(text::DISPLAY_GONE);
            }
            Ok(Ok(Ended::Closed)) => {
                let name = self.target.as_ref().map_or("", Target::name);
                log::info(text::target_closed(name));
                self.status.state = DisplayStatus::Waiting;
            }
            Ok(Ok(Ended::NoFrames)) => self.fall_back_to_monitor(),
            Ok(Ok(Ended::Unsupported(reason))) => {
                self.report_error(text::session_failed(reason));
                self.status.state = DisplayStatus::Unsupported;
            }
            Ok(Err(error)) => self.fail(format!("{error:#}")),
            Err(_) => self.fail(text::CAPTURE_PANICKED),
        }
    }

    fn fall_back_to_monitor(&mut self) {
        let Some(window @ Target::Window { .. }) = &self.target else {
            self.fail(text::NO_FRAMES);
            return;
        };
        log::info(text::falling_back_to_monitor(window.name()));
        let monitor = window.monitor();
        self.start_session(monitor);
    }

    fn retry(&mut self) {
        match self.config.mode {
            Mode::Desktop => {
                log::info(text::RESTARTING);
                self.restart_session();
            }
            Mode::Fullscreen => self.begin(),
        }
    }

    fn spawn_save(
        &self,
        snapshot: Snapshot,
        ending: Ending,
    ) -> io::Result<JoinHandle<Result<PathBuf, SaveError>>> {
        let directory = self.config.clip_directory();
        let write = self.services.write;
        thread::Builder::new()
            .name("save".to_owned())
            .spawn(move || {
                let _ending = ending;
                if let Err(error) = sys::thread::enter_background_mode() {
                    log::warning(text::background_save_failed(error));
                }
                write(&snapshot, &directory)
            })
    }

    fn tick_save(&mut self) {
        let now = Instant::now();
        let Some(saving) = self
            .saving
            .as_mut()
            .filter(|saving| now >= saving.next_tick)
        else {
            return;
        };
        saving.next_tick = now + PROGRESS_INTERVAL;
        (self.services.chime)(Tone::Progress);
    }

    fn finish_save(&mut self, saving: Saving) {
        match saving.join.join() {
            Ok(Ok(path)) => self.clip_saved(saving.seconds, path),
            Ok(Err(error)) => self.report_failed_save(text::save_failed(error)),
            Err(_) => self.report_failed_save(text::save_failed(text::SAVE_PANICKED)),
        }
    }

    fn clip_saved(&mut self, seconds: f64, path: PathBuf) {
        log::info(text::clip_saved(seconds, &path));
        self.ledger.add_saved(&path);
        self.status.last_clip = Some(path);
        self.status.error = None;
        (self.services.chime)(Tone::Saved);
    }

    fn report_failed_save(&mut self, message: String) {
        self.report_error(message);
        (self.services.chime)(Tone::Failed);
    }

    fn bank_live(&mut self) {
        if let Some(live) = self.live.take() {
            self.ledger.bank(&live);
        }
    }

    fn publish(&mut self) {
        let reading = self.read_live();
        self.mark_recording(&reading);
        self.describe_session(&reading);
        self.count_bytes(reading.live_bytes);
        self.measure_rate(reading.running, reading.sample);
        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = self.status.clone();
    }

    fn read_live(&self) -> Reading {
        let live = self.live.as_deref();
        let running = self.session.is_some();
        let stream = live.and_then(Live::stream);
        let live_bytes = live.map_or(0, Live::bytes);
        let (encoded, _) = live.map_or((0, 0), Live::counters);
        Reading {
            running,
            stream,
            live_bytes,
            held: live.map(Live::held),
            sample: Sample {
                at: Instant::now(),
                encoded,
            },
        }
    }

    fn mark_recording(&mut self, reading: &Reading) {
        if let Some(stream) = &reading.stream
            && reading.running
            && self.status.state == DisplayStatus::Starting
        {
            log::info(text::recording(stream, &self.config));
            self.status.state = DisplayStatus::Recording;
        }
    }

    fn describe_session(&mut self, reading: &Reading) {
        self.status.recording = reading.running;
        self.status.source = self.target.as_ref().map(|target| target.name().to_owned());
        self.status.backend = reading.running.then_some(self.config.backend);
        self.status.stream = reading.stream;
        self.status.bitrate_mbps = self.config.bitrate_mbps;
        self.status.buffer_held = reading.held.unwrap_or_default();
        self.status.buffer_memory = reading.held.map(|held| {
            // Nothing placed yet, or placed where asked: the setting; refused: RAM.
            if held.frames == 0 || held.stored {
                self.config.buffer_memory
            } else {
                Memory::Ram
            }
        });
        self.status.buffer_seconds = self.config.buffer_seconds;
        self.status.buffer_capacity_bytes = self.config.estimated_buffer_bytes();
    }

    fn count_bytes(&mut self, live_bytes: u64) {
        let totals = self.ledger.totals();
        self.status.session_written_bytes = self.ledger.session_buffered.saturating_add(live_bytes);
        self.status.session_saved_bytes = self.ledger.session_saved;
        self.status.saved_bytes = totals.map(|totals| totals.saved_bytes);
        self.status.buffered_lifetime_bytes =
            totals.map(|totals| totals.buffered_bytes.saturating_add(live_bytes));
    }

    fn measure_rate(&mut self, running: bool, current: Sample) {
        let Some(previous) = self.sample.filter(|_| running) else {
            self.status.encode_rate = None;
            self.sample = running.then_some(current);
            return;
        };
        let elapsed = current.at.duration_since(previous.at);
        let span = if self.status.encode_rate.is_some() {
            RATE_SPAN
        } else {
            FIRST_RATE_SPAN
        };
        if elapsed < span {
            return;
        }
        self.status.encode_rate =
            Some((current.encoded - previous.encoded) as f64 / elapsed.as_secs_f64());
        self.sample = Some(current);
    }
}

fn load_totals(store: &Store) -> Option<Totals> {
    store
        .load_totals()
        .inspect_err(|error| log::error(text::load_totals_failed(log::chain(error))))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{Matrix, Packet, Picture};
    use std::sync::atomic::AtomicUsize;

    #[expect(clippy::unnecessary_wraps)]
    fn steady(config: &Config, _: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
        live.set_stream(Stream {
            codec: config.codec,
            width: 64,
            height: 64,
            frame_rate: config.frame_rate,
            matrix: Matrix::Bt709,
        });
        let mut index = 0;
        while !stop.load(Ordering::Relaxed) {
            let keyframe = index % 10 == 0;
            let data: &[u8] = if keyframe {
                &[
                    0, 0, 0, 1, 0x67, 0x64, 0, 0x1F, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65, 0x88,
                ]
            } else {
                &[0, 0, 0, 1, 0x41, 0x9A]
            };
            live.push(
                Packet {
                    index,
                    keyframe,
                    data: data.into(),
                },
                Picture::Fresh,
            );
            index += 1;
            thread::sleep(Duration::from_millis(1));
        }
        Ok(Ended::Stopped)
    }

    fn failing(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
        anyhow::bail!("no capture device")
    }

    #[expect(clippy::unnecessary_wraps)]
    fn unsupported(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
        Ok(Ended::Unsupported(
            "this GPU's NVENC has no AV1 encoder".to_owned(),
        ))
    }

    #[expect(clippy::unnecessary_wraps)]
    fn closing(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
        Ok(Ended::Closed)
    }

    static SESSIONS: AtomicUsize = AtomicUsize::new(0);

    fn scripted(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
        SESSIONS.fetch_add(1, Ordering::Relaxed);
        match target {
            Target::Window { window: 1, .. } => {
                steady_until(config, live, stop, Duration::from_millis(1500));
                Ok(Ended::Closed)
            }
            Target::Window { .. } => Ok(Ended::NoFrames),
            Target::Monitor { .. } => steady(config, target, live, stop),
        }
    }

    fn steady_until(config: &Config, live: &Live, stop: &AtomicBool, span: Duration) {
        let ended = AtomicBool::new(false);
        let deadline = Instant::now() + span;
        thread::scope(|scope| {
            scope.spawn(|| {
                while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(5));
                }
                ended.store(true, Ordering::Relaxed);
            });
            let _ = steady(config, &Target::primary_monitor(), live, &ended);
        });
    }

    fn services(record: Recorder, retry_interval: Duration) -> Services {
        Services {
            record,
            audio: |_| None,
            write: replay::save,
            chime: Box::new(|_| {}),
            retry_interval,
        }
    }

    fn spawn(
        record: Recorder,
        retry_interval: Duration,
        store: Option<Store>,
        config: Config,
    ) -> Worker {
        Worker::spawn_with(services(record, retry_interval), store, config).unwrap()
    }

    fn wait_for(worker: &Worker, done: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            worker.refresh();
            let status = worker.status();
            if done(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last status {status:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn desktop() -> Config {
        Config {
            mode: Mode::Desktop,
            ..Config::default()
        }
    }

    fn config_in(directory: &Path) -> Config {
        Config {
            clip_directory: Some(directory.display().to_string()),
            ..desktop()
        }
    }

    fn window(window: isize) -> Target {
        Target::Window {
            window,
            monitor: 7,
            name: format!("game{window}.exe"),
        }
    }

    #[test]
    fn desktop_mode_records_and_saves_a_clip() {
        let directory = tempfile::tempdir().unwrap();
        let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));

        let status = wait_for(&worker, |status| {
            status.state == DisplayStatus::Recording && status.session_written_bytes > 0
        });
        assert!(status.recording);
        assert_eq!(status.stream.unwrap().frame_rate, 60);
        assert_eq!(status.source.as_deref(), Some("the primary monitor"));

        worker.save();
        let status = wait_for(&worker, |status| status.last_clip.is_some());
        let clip = status.last_clip.unwrap();
        assert!(clip.starts_with(directory.path()));
        assert_eq!(&std::fs::read(&clip).unwrap()[4..8], b"ftyp");
    }

    #[test]
    fn a_thread_that_ends_wakes_the_worker_without_a_refresh() {
        let worker = spawn(closing, RETRY_INTERVAL, None, desktop());
        thread::sleep(Duration::from_millis(300));
        let status = worker.status();
        assert_eq!(status.state, DisplayStatus::Retrying, "{status:?}");
        assert!(status.error.unwrap().contains(text::DISPLAY_GONE));

        let directory = tempfile::tempdir().unwrap();
        let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));
        wait_for(&worker, |status| status.session_written_bytes > 0);
        worker.save();
        thread::sleep(Duration::from_millis(500));
        let status = worker.status();
        assert!(status.last_clip.is_some(), "{status:?}");
    }

    #[test]
    fn the_lifetime_totals_grow_and_survive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let clips = directory.path().join("clips");
        let config = config_in(&clips);
        let store = Store::at(state.clone()).unwrap();
        let worker = spawn(steady, RETRY_INTERVAL, Some(store), config.clone());
        let status = wait_for(&worker, |status| status.saved_bytes.is_some());
        assert_eq!(status.saved_bytes, Some(0));

        worker.save();
        let status = wait_for(&worker, |status| status.last_clip.is_some());
        let clip_bytes = std::fs::metadata(status.last_clip.unwrap()).unwrap().len();
        assert_eq!(status.saved_bytes, Some(clip_bytes));
        let buffered = status.buffered_lifetime_bytes.unwrap();
        assert!(buffered > 0, "the recording went into the buffer");
        drop(worker);

        let store = Store::at(state).unwrap();
        let worker = spawn(steady, RETRY_INTERVAL, Some(store), config);
        let status = wait_for(&worker, |status| status.saved_bytes.is_some());
        assert_eq!(status.saved_bytes, Some(clip_bytes));
        assert!(status.buffered_lifetime_bytes.unwrap() >= buffered);
    }

    #[test]
    fn pausing_keeps_the_buffer_saveable_and_resuming_records_again() {
        let directory = tempfile::tempdir().unwrap();
        let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));
        wait_for(&worker, |status| {
            status.state == DisplayStatus::Recording && status.session_written_bytes > 0
        });

        worker.pause();
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Paused);
        assert!(!status.recording);
        worker.save();
        let status = wait_for(&worker, |status| status.last_clip.is_some());
        assert_eq!(status.state, DisplayStatus::Paused);

        worker.resume();
        wait_for(&worker, |status| status.state == DisplayStatus::Recording);
        let before = worker.status().session_written_bytes;
        worker.restart();
        wait_for(&worker, |status| {
            status.state == DisplayStatus::Recording && status.session_written_bytes > before
        });
    }

    #[test]
    fn a_failed_session_is_retried() {
        let worker = spawn(failing, Duration::from_millis(50), None, desktop());

        let status = wait_for(&worker, |status| status.state == DisplayStatus::Retrying);
        assert!(status.error.unwrap().contains("no capture device"));

        worker.save();
        wait_for(&worker, |status| status.nothing_to_save);
    }

    #[test]
    fn an_unsupported_configuration_is_not_retried() {
        let worker = spawn(unsupported, Duration::from_millis(10), None, desktop());

        let status = wait_for(&worker, |status| status.state == DisplayStatus::Unsupported);
        assert!(status.error.unwrap().contains("AV1"));
        assert!(!status.recording);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(worker.status().state, DisplayStatus::Unsupported);
    }

    #[test]
    fn full_screen_mode_waits_for_an_app_and_follows_it() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            mode: Mode::Fullscreen,
            ..config_in(directory.path())
        };
        SESSIONS.store(0, Ordering::Relaxed);
        let worker = spawn(scripted, RETRY_INTERVAL, None, config);

        let status = wait_for(&worker, |status| status.state == DisplayStatus::Waiting);
        assert!(!status.recording);
        assert!(status.source.is_none());

        worker.target(window(1));
        wait_for(&worker, |status| status.state == DisplayStatus::Recording);
        worker.target(window(1));
        worker.target(window(1));
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Waiting);
        assert_eq!(SESSIONS.load(Ordering::Relaxed), 1);
        assert_eq!(status.source.as_deref(), Some("game1.exe"));
        worker.save();
        wait_for(&worker, |status| status.last_clip.is_some());

        worker.target(window(2));
        let status = wait_for(&worker, |status| {
            status.state == DisplayStatus::Recording && status.session_written_bytes > 0
        });
        assert_eq!(SESSIONS.load(Ordering::Relaxed), 3);
        assert_eq!(status.source.as_deref(), Some("game2.exe"));
    }

    #[test]
    fn full_screen_mode_ignores_targets_while_stopped_or_cooling_down() {
        let config = Config {
            mode: Mode::Fullscreen,
            ..Config::default()
        };
        let worker = spawn(failing, Duration::from_millis(100), None, config);
        wait_for(&worker, |status| status.state == DisplayStatus::Waiting);

        worker.target(window(1));
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Retrying);
        assert!(status.error.is_some());
        worker.target(window(1));
        thread::sleep(Duration::from_millis(20));
        assert_eq!(worker.status().state, DisplayStatus::Retrying);
        wait_for(&worker, |status| status.state == DisplayStatus::Waiting);
    }

    #[test]
    fn applying_settings_persists_them_and_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("state")).unwrap();
        let worker = spawn(
            steady,
            RETRY_INTERVAL,
            Some(store),
            config_in(directory.path()),
        );
        wait_for(&worker, |status| status.state == DisplayStatus::Recording);

        let config = Config {
            frame_rate: 30,
            ..config_in(directory.path())
        };
        worker.start(config.clone());
        wait_for(&worker, |status| {
            status.stream.is_some_and(|stream| stream.frame_rate == 30)
        });
        drop(worker);

        let saved = Store::at(directory.path().join("state"))
            .unwrap()
            .load_config()
            .unwrap();
        assert_eq!(saved, config);
    }

    #[test]
    fn choosing_a_look_is_saved_and_leaves_recording_alone() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let config = config_in(directory.path());
        let worker = spawn(
            steady,
            RETRY_INTERVAL,
            Some(Store::at(state.clone()).unwrap()),
            config.clone(),
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.set_theme(Theme::Nova);
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Recording);
        assert!(status.recording, "the session carries on");
        drop(worker);

        let saved = Store::at(state).unwrap().load_config().unwrap();
        assert_eq!(
            saved,
            Config {
                theme: Theme::Nova,
                ..config
            }
        );
    }

    type Heard = Arc<Mutex<Vec<(Instant, Tone)>>>;

    fn spawn_listening(mut services: Services, write: Writer, config: Config) -> (Worker, Heard) {
        let heard = Heard::default();
        let ear = Arc::clone(&heard);
        services.write = write;
        services.chime = Box::new(move |tone| {
            ear.lock().unwrap().push((Instant::now(), tone));
        });
        (Worker::spawn_with(services, None, config).unwrap(), heard)
    }

    fn tones(heard: &Heard) -> Vec<Tone> {
        heard
            .lock()
            .unwrap()
            .iter()
            .map(|&(_, tone)| tone)
            .collect()
    }

    fn assert_ticks_then(heard: &Heard, outcome: Tone) -> usize {
        let heard = heard.lock().unwrap();
        let (last, blips) = heard.split_last().expect("a tone played");
        assert_eq!(last.1, outcome, "the outcome comes last: {heard:?}");
        assert!(blips.iter().all(|&(_, tone)| tone == Tone::Progress));
        for pair in blips.windows(2) {
            let gap = pair[1].0.duration_since(pair[0].0);
            assert!(gap >= Duration::from_millis(900), "blips {gap:?} apart");
        }
        blips.len()
    }

    fn slow_write(snapshot: &Snapshot, directory: &Path) -> Result<PathBuf, SaveError> {
        thread::sleep(Duration::from_millis(2500));
        replay::save(snapshot, directory)
    }

    fn failing_write(_: &Snapshot, directory: &Path) -> Result<PathBuf, SaveError> {
        thread::sleep(Duration::from_millis(1500));
        Err(SaveError::Io {
            step: "create",
            path: directory.to_owned(),
            source: io::Error::other("the disk is full"),
        })
    }

    #[test]
    fn a_slow_save_ticks_each_second_then_sounds_saved_once() {
        let directory = tempfile::tempdir().unwrap();
        let (worker, heard) = spawn_listening(
            services(steady, RETRY_INTERVAL),
            slow_write,
            config_in(directory.path()),
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.save();
        wait_for(&worker, |_| !tones(&heard).is_empty());
        assert_eq!(tones(&heard), [Tone::Progress], "the first blip is at once");
        wait_for(&worker, |status| status.last_clip.is_some());
        let blips = assert_ticks_then(&heard, Tone::Saved);
        assert!((2..=3).contains(&blips), "{blips} blips in 2.5 seconds");

        thread::sleep(PROGRESS_INTERVAL + Duration::from_millis(300));
        assert_eq!(tones(&heard).len(), blips + 1);
    }

    #[test]
    fn a_failed_save_sounds_failed_and_stops_ticking() {
        let directory = tempfile::tempdir().unwrap();
        let (worker, heard) = spawn_listening(
            services(steady, RETRY_INTERVAL),
            failing_write,
            config_in(directory.path()),
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.save();
        let status = wait_for(&worker, |status| status.error.is_some());
        assert!(status.error.unwrap().contains("the disk is full"));
        assert!(status.last_clip.is_none());
        let blips = assert_ticks_then(&heard, Tone::Failed);
        assert!((1..=2).contains(&blips), "{blips} blips in 1.5 seconds");

        thread::sleep(PROGRESS_INTERVAL + Duration::from_millis(300));
        assert_eq!(tones(&heard).len(), blips + 1);
    }

    #[test]
    fn nothing_to_save_sounds_failed_without_ticking() {
        let (worker, heard) = spawn_listening(
            services(failing, Duration::from_secs(60)),
            slow_write,
            desktop(),
        );
        wait_for(&worker, |status| status.state == DisplayStatus::Retrying);

        worker.save();
        wait_for(&worker, |status| status.nothing_to_save);
        assert_eq!(tones(&heard), [Tone::Failed]);
    }
}
