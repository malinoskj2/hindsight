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
    any::Any,
    fmt::Display,
    io,
    num::NonZeroIsize,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SendError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) mod text;

const RATE_SPAN: Duration = Duration::from_secs(10);
const FIRST_RATE_SPAN: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// A monitor's OS handle, never null; carried as an integer so the worker stays portable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MonitorHandle(NonZeroIsize);

/// A window's OS handle, never null; carried as an integer so the worker stays portable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowHandle(NonZeroIsize);

impl MonitorHandle {
    /// `None` for a null handle.
    pub(crate) fn new(raw: isize) -> Option<Self> {
        NonZeroIsize::new(raw).map(Self)
    }

    pub(crate) fn get(self) -> isize {
        self.0.get()
    }
}

impl WindowHandle {
    /// `None` for a null handle.
    pub(crate) fn new(raw: isize) -> Option<Self> {
        NonZeroIsize::new(raw).map(Self)
    }

    pub(crate) fn get(self) -> isize {
        self.0.get()
    }
}

/// What to capture. Two targets are the same when their handles are; the name only describes.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// `None` is the primary monitor.
    Monitor {
        handle: Option<MonitorHandle>,
        name: String,
    },
    Window {
        window: WindowHandle,
        monitor: MonitorHandle,
        name: String,
    },
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Monitor { handle: a, .. }, Self::Monitor { handle: b, .. }) => a == b,
            (
                Self::Window {
                    window: a,
                    monitor: a_monitor,
                    ..
                },
                Self::Window {
                    window: b,
                    monitor: b_monitor,
                    ..
                },
            ) => a == b && a_monitor == b_monitor,
            _ => false,
        }
    }
}

impl Eq for Target {}

impl Target {
    pub(crate) fn primary_monitor() -> Self {
        Self::Monitor {
            handle: None,
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
                handle: Some(*monitor),
                name: name.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Stopped,
    #[cfg_attr(all(not(windows), test), expect(dead_code))]
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
/// The worker's sense of time: retries and save ticks fall due by it.
type Clock = Box<dyn Fn() -> Instant + Send>;

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
    clock: Clock,
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
            clock: Box::new(Instant::now),
        }
    }
}

/// Where the worker is; what it shows is built from this when it publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Waiting,
    Recording,
    Paused,
    Retrying,
    Unsupported,
}

/// What the worker is doing, carrying what only that state has.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) enum State {
    /// Capture is running and no frame has arrived yet.
    Starting(Running),
    Recording(Running),
    /// Full-screen mode, with no full-screen app to record.
    Waiting,
    /// Capture stopped on request; what was buffered stays saveable until it resumes.
    Paused,
    /// Capture failed and is tried again shortly.
    Retrying,
    /// Capture fails the same way each time with these settings.
    Unsupported,
    /// Nothing is captured: before the worker first reports, or once it has shut down.
    #[default]
    Idle,
    /// The worker itself failed; nothing is recorded until the app starts again.
    Stopped,
}

impl State {
    /// The capture session, while one runs.
    pub(crate) fn running(&self) -> Option<&Running> {
        match self {
            Self::Starting(running) | Self::Recording(running) => Some(running),
            _ => None,
        }
    }
}

/// A capture session that is running.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Running {
    pub(crate) backend: Backend,
    /// Frames encoded a second, once measured.
    pub(crate) encode_rate: Option<f64>,
}

/// The replay buffer, while there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Buffer {
    /// Known once the first frame arrives.
    pub(crate) stream: Option<Stream>,
    /// What the buffer holds now.
    pub(crate) held: Held,
    /// Where it is kept.
    pub(crate) memory: Memory,
}

/// Whether system sound goes into the buffer, while capture runs and the settings ask for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sound {
    Recording,
    /// Sound capture could not start or has stopped; the log says why.
    Failed,
}

/// The bytes buffered and saved, this run and over the app's lifetime.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Bytes {
    pub(crate) session_written: u64,
    pub(crate) session_saved: u64,
    /// `None` when the lifetime totals could not be read.
    pub(crate) lifetime_written: Option<u64>,
    pub(crate) lifetime_saved: Option<u64>,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Status {
    pub(crate) state: State,
    /// The display or app recorded last; kept until another is chosen.
    pub(crate) source: Option<String>,
    pub(crate) buffer: Option<Buffer>,
    pub(crate) sound: Option<Sound>,
    /// The settings' bitrate, buffer length and the buffer size they come to.
    pub(crate) bitrate_mbps: u32,
    pub(crate) buffer_seconds: u32,
    pub(crate) buffer_capacity_bytes: u64,
    pub(crate) bytes: Bytes,
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
    /// Pause while capturing, resume while paused: the worker knows which it is.
    TogglePause,
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
    #[cfg_attr(all(not(windows), test), expect(dead_code))]
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
                let machine = Machine::new(services, store, config, Arc::clone(&output), wake);
                run_guarded(machine, &receiver, &output);
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

    pub(crate) fn toggle_pause(&self) {
        self.request(Command::TogglePause);
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
        if let Err(SendError(command)) = self.commands.send(command)
            && !matches!(command, Command::Wake | Command::Exit)
        {
            log::warning(text::WORKER_GONE);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.request(Command::Exit);
        if let Some(join) = self.join.take()
            && let Err(payload) = join.join()
        {
            log::error(text::worker_panicked(panic_message(&*payload)));
        }
    }
}

struct Session {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    join: JoinHandle<Result<Ended>>,
    /// `None` when sound is not asked for or its capture could not start.
    audio: Option<audio::Capture>,
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
    phase: Phase,
    error: Option<String>,
    /// A save was asked for before the buffer held anything; cleared when one starts.
    nothing_to_save: bool,
    last_clip: Option<PathBuf>,
    encode_rate: Option<f64>,
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
            phase: Phase::Starting,
            error: None,
            nothing_to_save: false,
            last_clip: None,
            encode_rate: None,
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
                Ok(Command::TogglePause) => self.toggle_pause(),
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

    fn now(&self) -> Instant {
        (self.services.clock)()
    }

    fn receive(&self, commands: &Receiver<Command>) -> Result<Command, RecvTimeoutError> {
        let tick = self.saving.as_ref().map(|saving| saving.next_tick);
        match self.retry_at.into_iter().chain(tick).min() {
            Some(due) => commands.recv_timeout(due.saturating_duration_since(self.now())),
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
        if self.save_settings(&config).is_err() {
            return;
        }
        log::info(text::settings_applied(&config));
        self.config = config;
        self.paused = false;
        self.stop_session();
        self.begin();
    }

    fn toggle_pause(&mut self) {
        if self.paused {
            self.resume();
        } else {
            self.pause();
        }
    }

    /// Stops capturing but keeps the buffer, so a clip can still be saved from before the pause.
    fn pause(&mut self) {
        log::info(text::PAUSED);
        self.paused = true;
        self.retry_at = None;
        self.stop_session();
        self.phase = Phase::Paused;
        self.error = None;
    }

    fn resume(&mut self) {
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
            || self.phase == Phase::Unsupported
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
            self.nothing_to_save = true;
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
                    next_tick: self.now() + PROGRESS_INTERVAL,
                });
                self.nothing_to_save = false;
            }
            Err(error) => self.report_failed_save(text::save_failed(error)),
        }
    }

    fn remember_theme(&mut self, theme: Theme) {
        let config = Config {
            theme,
            ..self.config.clone()
        };
        // The failure is reported; the look still changes for this run.
        let _ = self.save_settings(&config);
        self.config = config;
    }

    /// Reports a failure on the Status tab as well as returning it.
    fn save_settings(&mut self, config: &Config) -> Result<(), StorageError> {
        self.ledger
            .save_config(config)
            .inspect_err(|error| self.report_error(text::save_settings_failed(log::chain(error))))
    }

    fn begin(&mut self) {
        self.retry_at = None;
        match self.config.mode {
            Mode::Desktop => self.start_session(Target::primary_monitor()),
            Mode::Fullscreen => {
                self.target = None;
                self.phase = Phase::Waiting;
                self.error = None;
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
                    audio,
                });
                self.live = Some(live);
                self.phase = Phase::Starting;
                self.error = None;
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
            Ok(Err(error)) => log::error(text::session_failed(format_args!("{error:#}"))),
            Err(payload) => log::error(text::session_failed(text::capture_panicked(
                panic_message(&*payload),
            ))),
        }
    }

    fn fail(&mut self, error: impl Display) {
        self.report_error(text::session_failed(error));
        self.phase = Phase::Retrying;
        self.retry_at = Some(self.now() + self.services.retry_interval);
    }

    fn report_error(&mut self, message: impl Display) {
        let message = message.to_string();
        log::error(&message);
        self.error = Some(message);
    }

    fn poll(&mut self) {
        if let Some(session) = self
            .session
            .take_if(|session| session.done.load(Ordering::Acquire))
        {
            self.session_ended(session.join.join());
        }

        if self.retry_at.is_some_and(|due| self.now() >= due) {
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
                self.phase = Phase::Waiting;
            }
            Ok(Ok(Ended::NoFrames)) => self.fall_back_to_monitor(),
            Ok(Ok(Ended::Unsupported(reason))) => {
                self.report_error(text::session_failed(reason));
                self.phase = Phase::Unsupported;
            }
            Ok(Err(error)) => self.fail(format_args!("{error:#}")),
            Err(payload) => self.fail(text::capture_panicked(panic_message(&*payload))),
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
        let now = self.now();
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
            Err(payload) => self.report_failed_save(text::save_failed(text::save_panicked(
                panic_message(&*payload),
            ))),
        }
    }

    fn clip_saved(&mut self, seconds: f64, path: PathBuf) {
        log::info(text::clip_saved(seconds, &path));
        self.ledger.add_saved(&path);
        self.last_clip = Some(path);
        self.error = None;
        (self.services.chime)(Tone::Saved);
    }

    fn report_failed_save(&mut self, message: impl Display) {
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
        self.measure_rate(reading.running, reading.sample);
        let status = self.describe(&reading);
        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = status;
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
                at: self.now(),
                encoded,
            },
        }
    }

    fn mark_recording(&mut self, reading: &Reading) {
        if let Some(stream) = &reading.stream
            && reading.running
            && self.phase == Phase::Starting
        {
            log::info(text::recording(stream, &self.config));
            self.phase = Phase::Recording;
        }
    }

    fn describe(&self, reading: &Reading) -> Status {
        Status {
            state: self.state(reading.running),
            source: self.target.as_ref().map(|target| target.name().to_owned()),
            buffer: reading.held.map(|held| Buffer {
                stream: reading.stream,
                held,
                // Nothing placed yet, or placed where asked: the setting; refused: RAM.
                memory: if held.frames == 0 || held.stored {
                    self.config.buffer_memory
                } else {
                    Memory::Ram
                },
            }),
            sound: self.sound(),
            bitrate_mbps: self.config.bitrate_mbps,
            buffer_seconds: self.config.buffer_seconds,
            buffer_capacity_bytes: self.config.estimated_buffer_bytes(),
            bytes: self.count_bytes(reading.live_bytes),
            last_clip: self.last_clip.clone(),
            error: self.error.clone(),
            nothing_to_save: self.nothing_to_save,
        }
    }

    fn sound(&self) -> Option<Sound> {
        let session = self.session.as_ref().filter(|_| self.config.audio)?;
        Some(match &session.audio {
            Some(capture) if !capture.has_stopped() => Sound::Recording,
            _ => Sound::Failed,
        })
    }

    fn state(&self, running: bool) -> State {
        let session = running.then_some(Running {
            backend: self.config.backend,
            encode_rate: self.encode_rate,
        });
        match (self.phase, session) {
            (Phase::Starting, Some(session)) => State::Starting(session),
            (Phase::Recording, Some(session)) => State::Recording(session),
            (Phase::Starting | Phase::Recording, None) => State::Idle,
            (Phase::Waiting, _) => State::Waiting,
            (Phase::Paused, _) => State::Paused,
            (Phase::Retrying, _) => State::Retrying,
            (Phase::Unsupported, _) => State::Unsupported,
        }
    }

    fn count_bytes(&self, live_bytes: u64) -> Bytes {
        let totals = self.ledger.totals();
        Bytes {
            session_written: self.ledger.session_buffered.saturating_add(live_bytes),
            session_saved: self.ledger.session_saved,
            lifetime_written: totals.map(|totals| totals.buffered_bytes.saturating_add(live_bytes)),
            lifetime_saved: totals.map(|totals| totals.saved_bytes),
        }
    }

    fn measure_rate(&mut self, running: bool, current: Sample) {
        let Some(previous) = self.sample.filter(|_| running) else {
            self.encode_rate = None;
            self.sample = running.then_some(current);
            return;
        };
        let elapsed = current.at.duration_since(previous.at);
        let span = if self.encode_rate.is_some() {
            RATE_SPAN
        } else {
            FIRST_RATE_SPAN
        };
        if elapsed < span {
            return;
        }
        self.encode_rate =
            Some((current.encoded - previous.encoded) as f64 / elapsed.as_secs_f64());
        self.sample = Some(current);
    }
}

/// Runs the worker; should it panic, says why on the Status tab and stops what it can.
fn run_guarded(mut machine: Machine, commands: &Receiver<Command>, output: &Mutex<Status>) {
    let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| machine.run(commands))) else {
        return;
    };
    let message = text::worker_panicked(panic_message(&*payload)).to_string();
    log::error(&message);
    // Stops capture and finishes a save if it can; a second panic is left at that.
    let _ = panic::catch_unwind(AssertUnwindSafe(|| machine.shut_down()));
    let mut status = output.lock().unwrap_or_else(PoisonError::into_inner);
    status.state = State::Stopped;
    status.error = Some(message);
}

/// What a panicking thread said, when it said it with a string.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(text::NO_PANIC_MESSAGE)
}

fn load_totals(store: &Store) -> Option<Totals> {
    store
        .load_totals()
        .inspect_err(|error| log::error(text::load_totals_failed(log::chain(error))))
        .ok()
}

#[cfg(test)]
mod tests;
