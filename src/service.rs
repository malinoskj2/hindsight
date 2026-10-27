//! The recording worker: keeps one capture session running, saves clips from its buffer and
//! screenshots of what it captures.

use crate::{
    audio::{self, Sources, StandIns},
    config::{Backend, Config, Container, Memory, Mode, Theme},
    log,
    replay::{self, Held, Live, SaveError, Saved, Snapshot, Stream},
    screenshot::{self, Readback, Reply, ScreenshotError, Taken},
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

/// A monitor's OS handle, which is never null. It is carried as an integer so the worker stays
/// portable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MonitorHandle(NonZeroIsize);

/// A window's OS handle, which is never null. It is carried as an integer so the worker stays
/// portable.
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

/// The capture target. Equality compares handles only: the name is descriptive, and a window is
/// the same target however it is recorded.
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
        title: String,
        /// The client size, which covers the monitor.
        size: (u32, u32),
        /// The monitor is turned from the panel's own orientation.
        rotated: bool,
        through: Through,
    },
}

/// How a window target is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Through {
    /// The window's own content, where the backend can capture a window, and its monitor
    /// otherwise.
    Window,
    /// The monitor the window fills, as when the window cannot be captured itself.
    Monitor,
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

    /// The same target, recorded through its monitor.
    fn through_monitor(&self) -> Self {
        let mut target = self.clone();
        if let Self::Window { through, .. } = &mut target {
            *through = Through::Monitor;
        }
        target
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Stopped,
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture reports it")
    )]
    Resized,
    Closed,
    /// The app presents in a way Windows cannot compose, so its monitor must be captured instead.
    NoFrames,
    /// Trying again would fail the same way.
    Unsupported(String),
}

type Recorder = fn(&Config, &Target, &Live, &AtomicBool) -> Result<Ended>;
type Audio = fn(&Arc<Live>, Sources) -> Option<audio::Capture>;
type Writer = fn(&Snapshot, &Path, Container) -> Result<Saved, SaveError>;
type Chime = Box<dyn Fn(Tone) + Send>;
/// The clock the worker uses to schedule retries and save ticks.
type Clock = Box<dyn Fn() -> Instant + Send>;

#[cfg(windows)]
const RECORDER: Recorder = crate::gpu::record;
#[cfg(not(windows))]
const RECORDER: Recorder = |_, _, _, _| anyhow::bail!("capture requires Windows");

/// The services the worker depends on. Tests replace any of them with a stand-in.
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

/// The worker's current phase. The published status is derived from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Waiting,
    Recording,
    Paused,
    Retrying,
    Unsupported,
}

/// The worker's state as published, carrying the data specific to each state.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) enum State {
    /// Capture is running and no frame has arrived yet.
    Starting(Running),
    Recording(Running),
    /// Full-screen mode, with no full-screen app to record.
    Waiting,
    /// Capture was stopped on request. What was buffered can still be saved until capture
    /// resumes.
    Paused,
    /// Capture failed and is tried again shortly.
    Retrying,
    /// Capture fails the same way each time with these settings.
    Unsupported,
    /// Nothing is captured, either before the worker first reports or once it has shut down.
    #[default]
    Idle,
    /// The worker itself failed. Nothing is recorded until the app is started again.
    Stopped,
}

impl State {
    pub(crate) fn running(&self) -> Option<&Running> {
        match self {
            Self::Starting(running) | Self::Recording(running) => Some(running),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Running {
    pub(crate) backend: Backend,
    /// Frames encoded per second, once measured.
    pub(crate) encode_rate: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Buffer {
    /// Known once the first frame arrives.
    pub(crate) stream: Option<Stream>,
    pub(crate) held: Held,
    pub(crate) memory: Memory,
}

/// Whether sound is being recorded into the buffer, while capture runs and the settings enable
/// system sound or the microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sound {
    /// Recording, from the default device where a chosen one is missing.
    Recording(StandIns),
    /// Sound capture could not start or has stopped. The log gives the reason.
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
    /// The display or app recorded most recently, kept until another is chosen.
    pub(crate) source: Option<String>,
    pub(crate) buffer: Option<Buffer>,
    pub(crate) sound: Option<Sound>,
    /// The settings' bitrate and buffer length, and the buffer size they amount to.
    pub(crate) bitrate_mbps: u32,
    pub(crate) buffer_seconds: u32,
    pub(crate) buffer_capacity_bytes: u64,
    pub(crate) bytes: Bytes,
    pub(crate) last_clip: Option<PathBuf>,
    pub(crate) last_screenshot: Option<PathBuf>,
    /// The last clip was saved without the sound that was buffered for it.
    pub(crate) clip_without_sound: bool,
    /// What went wrong last, until the same work next succeeds.
    pub(crate) problem: Option<Problem>,
    /// A save was requested before the buffer held anything.
    pub(crate) nothing_to_save: bool,
}

/// Something that failed, as the Status tab reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) kind: ProblemKind,
    /// The local time it failed, as `HH:MM`.
    pub(crate) at: String,
    /// Why, as the log records it.
    pub(crate) cause: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProblemKind {
    ClipNotSaved,
    ScreenshotNotSaved,
    CaptureFailed,
    AppNotRecorded,
    SettingsNotSaved,
    WorkerStopped,
}

#[derive(Debug)]
enum Command {
    Start(Config),
    Target(Target),
    Save,
    Screenshot,
    /// The capture thread's answer to a screenshot request.
    Captured(Taken),
    /// Pauses while capturing and resumes while paused. The worker knows which state it is in.
    TogglePause,
    Restart,
    Theme(Theme),
    CloseToTray(bool),
    CheckForUpdates(bool),
    MoreOptions(bool),
    Wake,
    Exit,
}

pub(crate) struct Worker {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows window starts the real worker")
    )]
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

    pub(crate) fn screenshot(&self) {
        self.request(Command::Screenshot);
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

    pub(crate) fn set_close_to_tray(&self, close_to_tray: bool) {
        self.request(Command::CloseToTray(close_to_tray));
    }

    pub(crate) fn set_check_for_updates(&self, check_for_updates: bool) {
        self.request(Command::CheckForUpdates(check_for_updates));
    }

    pub(crate) fn set_more_options(&self, more_options: bool) {
        self.request(Command::MoreOptions(more_options));
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
    /// `None` when sound is not requested or its capture could not start.
    audio: Option<audio::Capture>,
}

struct Saving {
    seconds: f64,
    done: Arc<AtomicBool>,
    join: JoinHandle<Result<Saved, SaveError>>,
    next_tick: Instant,
}

/// A screenshot under way: asked of the capture thread, then written on a thread of its own.
enum Shot {
    Asked,
    Writing(Writing),
}

struct Writing {
    done: Arc<AtomicBool>,
    join: JoinHandle<Result<PathBuf, ScreenshotError>>,
}

/// Dropped when a capture or save thread's closure ends. Marks the thread done and wakes the
/// worker. `JoinHandle::is_finished` is not enough, as it returns false until the result is
/// stored.
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

    fn save_config(&self, previous: &Config, config: &Config) -> Result<(), StorageError> {
        self.store
            .as_ref()
            .map_or(Ok(()), |store| store.save_config(previous, config))
    }

    /// Adds a finished replay buffer to the totals.
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
    problem: Option<Problem>,
    /// A save was requested before the buffer held anything. Cleared when a save starts.
    nothing_to_save: bool,
    last_clip: Option<PathBuf>,
    last_screenshot: Option<PathBuf>,
    clip_without_sound: bool,
    encode_rate: Option<f64>,
    session: Option<Session>,
    target: Option<Target>,
    /// The last full-screen app that could not be recorded. It is not offered again until the
    /// settings change or capture restarts.
    passed_over: Option<Target>,
    live: Option<Arc<Live>>,
    paused: bool,
    retry_at: Option<Instant>,
    saving: Option<Saving>,
    shot: Option<Shot>,
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
            problem: None,
            nothing_to_save: false,
            last_clip: None,
            last_screenshot: None,
            clip_without_sound: false,
            encode_rate: None,
            session: None,
            target: None,
            passed_over: None,
            live: None,
            paused: false,
            retry_at: None,
            saving: None,
            shot: None,
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
                Ok(Command::Screenshot) => self.screenshot(),
                Ok(Command::Captured(taken)) => self.captured(taken),
                Ok(Command::TogglePause) => self.toggle_pause(),
                Ok(Command::Restart) => self.restart(),
                Ok(Command::Theme(theme)) => self.remember(Config {
                    theme,
                    ..self.config.clone()
                }),
                Ok(Command::CloseToTray(close_to_tray)) => self.remember(Config {
                    close_to_tray,
                    ..self.config.clone()
                }),
                Ok(Command::CheckForUpdates(check_for_updates)) => self.remember(Config {
                    check_for_updates,
                    ..self.config.clone()
                }),
                Ok(Command::MoreOptions(more_options)) => self.remember(Config {
                    more_options,
                    ..self.config.clone()
                }),
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
        if let Some(Shot::Writing(writing)) = self.shot.take() {
            self.finish_shot(writing);
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
        self.problem = None;
    }

    fn resume(&mut self) {
        log::info(text::RESUMED);
        self.paused = false;
        self.begin();
    }

    /// Stops the session and starts it again with the same target.
    fn restart(&mut self) {
        log::info(text::RESTARTING);
        self.paused = false;
        self.passed_over = None;
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
        let passed_over = self.passed_over.as_ref() == Some(&target);
        if same || passed_over {
            return;
        }
        if matches!(target, Target::Window { rotated: true, .. })
            && !self.config.backend.captures_rotated()
        {
            let reason = text::rotated_monitor(self.config.backend);
            self.pass_over(target, reason);
            return;
        }
        self.stop_session();
        log::info(text::target_found(&target));
        self.start_session(target);
    }

    /// Leaves a full-screen app unrecorded and waits for the next one. A session already
    /// recording another app carries on.
    fn pass_over(&mut self, target: Target, reason: impl Display) {
        let message = text::passed_over(target.name(), reason);
        if self.session.is_some() {
            log::warning(message);
        } else {
            self.report(ProblemKind::AppNotRecorded, message);
            self.phase = Phase::Waiting;
        }
        self.passed_over = Some(target);
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
            Err(error) => self.report_failed_save(ProblemKind::ClipNotSaved, error),
        }
    }

    fn screenshot(&mut self) {
        if self.shot.is_some() {
            log::info(text::ALREADY_SHOOTING);
            return;
        }
        let capturing = self.session.is_some();
        let Some(live) = self
            .live
            .as_ref()
            .filter(|live| capturing && live.stream().is_some())
        else {
            log::warning(text::NOTHING_TO_SHOOT);
            (self.services.chime)(Tone::Failed);
            return;
        };
        let wake = self.wake.clone();
        live.request_still(Reply::new(move |taken| {
            let _ = wake.send(Command::Captured(taken));
        }));
        self.shot = Some(Shot::Asked);
    }

    fn captured(&mut self, taken: Taken) {
        self.shot = None;
        let readback = match taken {
            Ok(readback) => readback,
            Err(error) => {
                self.report_failed_shot(log::chain(&error));
                return;
            }
        };
        let (done, ending) = self.ending();
        match self.spawn_shot(readback, ending) {
            Ok(join) => self.shot = Some(Shot::Writing(Writing { done, join })),
            Err(error) => self.report_failed_shot(error),
        }
    }

    fn spawn_shot(
        &self,
        readback: Box<dyn Readback>,
        ending: Ending,
    ) -> io::Result<JoinHandle<Result<PathBuf, ScreenshotError>>> {
        let directory = self.config.screenshot_directory();
        let format = self.config.screenshot_format;
        thread::Builder::new()
            .name("screenshot".to_owned())
            .spawn(move || {
                let _ending = ending;
                #[cfg(windows)]
                if let Err(error) = sys::thread::enter_background_mode() {
                    log::warning(text::background_save_failed(error));
                }
                screenshot::save(readback, &directory, format)
            })
    }

    fn finish_shot(&mut self, writing: Writing) {
        match writing.join.join() {
            Ok(Ok(path)) => {
                log::info(text::screenshot_saved(&path));
                self.last_screenshot = Some(path);
                self.problem = None;
                (self.services.chime)(Tone::Saved);
            }
            Ok(Err(error)) => self.report_failed_shot(log::chain(&error)),
            Err(payload) => {
                self.report_failed_shot(text::screenshot_panicked(panic_message(&*payload)));
            }
        }
    }

    fn report_failed_shot(&mut self, error: impl Display) {
        self.report_failed_save(ProblemKind::ScreenshotNotSaved, error);
    }

    /// Saves a change to a setting that leaves recording alone.
    fn remember(&mut self, config: Config) {
        // The failure is reported, and the change still holds for this run.
        let _ = self.save_settings(&config);
        self.config = config;
    }

    /// Reports a failure on the Status tab as well as returning it.
    fn save_settings(&mut self, config: &Config) -> Result<(), StorageError> {
        self.ledger
            .save_config(&self.config, config)
            .inspect_err(|error| self.report(ProblemKind::SettingsNotSaved, log::chain(error)))
    }

    fn begin(&mut self) {
        self.retry_at = None;
        self.passed_over = None;
        match self.config.mode {
            Mode::Desktop => self.start_session(Target::primary_monitor()),
            Mode::Fullscreen => {
                self.target = None;
                self.phase = Phase::Waiting;
                self.problem = None;
            }
        }
    }

    fn start_session(&mut self, target: Target) {
        self.bank_live();
        let live = Arc::new(Live::new(self.config.buffer_frames()));
        live.on_stream(self.waker());
        let stop = Arc::new(AtomicBool::new(false));
        let sources = sources(&self.config);
        let audio = sources
            .any()
            .then(|| (self.services.audio)(&live, sources))
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
                self.problem = None;
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
        let outcome = session.join.join();
        self.withdraw_still();
        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => log::error(text::session_failed(format_args!("{error:#}"))),
            Err(payload) => log::error(text::session_failed(text::capture_panicked(
                panic_message(&*payload),
            ))),
        }
    }

    /// Once the capture thread has ended, a screenshot it never took is answered as abandoned.
    fn withdraw_still(&self) {
        if let Some(live) = &self.live {
            live.withdraw_still();
        }
    }

    fn fail(&mut self, error: impl Display) {
        self.report(ProblemKind::CaptureFailed, error);
        self.phase = Phase::Retrying;
        self.retry_at = Some(self.now() + self.services.retry_interval);
    }

    fn report(&mut self, kind: ProblemKind, cause: impl Display) {
        let cause = cause.to_string();
        log::error(text::problem(kind, &cause));
        self.problem = Some(Problem {
            kind,
            at: log::clock(),
            cause,
        });
    }

    fn poll(&mut self) {
        if let Some(session) = self
            .session
            .take_if(|session| session.done.load(Ordering::Acquire))
        {
            let outcome = session.join.join();
            self.withdraw_still();
            self.session_ended(outcome);
        }

        if self.retry_at.is_some_and(|due| self.now() >= due) {
            self.retry();
        }

        // Checked before the tick, so no progress tone plays after the outcome.
        if let Some(saving) = self
            .saving
            .take_if(|saving| saving.done.load(Ordering::Acquire))
        {
            self.finish_save(saving);
        }
        self.tick_save();

        if let Some(Shot::Writing(writing)) = self.shot.take_if(
            |shot| matches!(shot, Shot::Writing(writing) if writing.done.load(Ordering::Acquire)),
        ) {
            self.finish_shot(writing);
        }
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
            // In full-screen mode the next app may be recordable, such as one on another monitor.
            Ok(Ok(Ended::Unsupported(reason))) if self.config.mode == Mode::Fullscreen => {
                match self.target.clone() {
                    Some(target) => self.pass_over(target, reason),
                    None => self.begin(),
                }
            }
            Ok(Ok(Ended::Unsupported(reason))) => {
                self.report(ProblemKind::CaptureFailed, reason);
                self.phase = Phase::Unsupported;
            }
            Ok(Err(error)) => self.fail(format_args!("{error:#}")),
            Err(payload) => self.fail(text::capture_panicked(panic_message(&*payload))),
        }
    }

    fn fall_back_to_monitor(&mut self) {
        let Some(
            window @ Target::Window {
                through: Through::Window,
                ..
            },
        ) = &self.target
        else {
            self.fail(text::NO_FRAMES);
            return;
        };
        log::info(text::falling_back_to_monitor(window.name()));
        let monitor = window.through_monitor();
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
    ) -> io::Result<JoinHandle<Result<Saved, SaveError>>> {
        let directory = self.config.video_directory();
        let container = self.config.container;
        let write = self.services.write;
        thread::Builder::new()
            .name("save".to_owned())
            .spawn(move || {
                let _ending = ending;
                #[cfg(windows)]
                if let Err(error) = sys::thread::enter_background_mode() {
                    log::warning(text::background_save_failed(error));
                }
                write(&snapshot, &directory, container)
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
            Ok(Ok(saved)) => self.clip_saved(saving.seconds, saved),
            Ok(Err(error)) => {
                self.report_failed_save(ProblemKind::ClipNotSaved, log::chain(&error));
            }
            Err(payload) => self.report_failed_save(
                ProblemKind::ClipNotSaved,
                text::save_panicked(panic_message(&*payload)),
            ),
        }
    }

    fn clip_saved(&mut self, seconds: f64, saved: Saved) {
        log::info(text::clip_saved(seconds, &saved.path));
        if let Some(error) = &saved.sound_error {
            log::warning(text::saved_without_sound(log::chain(error)));
        }
        self.clip_without_sound = saved.sound_error.is_some();
        self.ledger.add_saved(&saved.path);
        self.last_clip = Some(saved.path);
        self.problem = None;
        (self.services.chime)(Tone::Saved);
    }

    fn report_failed_save(&mut self, kind: ProblemKind, cause: impl Display) {
        self.report(kind, cause);
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
                // The configured memory while nothing has been placed yet or packets are placed
                // as configured, and RAM once the store has refused them.
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
            last_screenshot: self.last_screenshot.clone(),
            clip_without_sound: self.clip_without_sound,
            problem: self.problem.clone(),
            nothing_to_save: self.nothing_to_save,
        }
    }

    fn sound(&self) -> Option<Sound> {
        let session = self
            .session
            .as_ref()
            .filter(|_| sources(&self.config).any())?;
        Some(match &session.audio {
            Some(capture) if !capture.has_stopped() => Sound::Recording(capture.stand_ins()),
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

/// Runs the worker. If it panics, reports the reason on the Status tab and stops what it can.
fn sources(config: &Config) -> Sources {
    Sources {
        system: config.audio,
        microphone: config.microphone,
        system_device: config.sound_device.clone(),
        microphone_device: config.microphone_device.clone(),
    }
}

fn run_guarded(mut machine: Machine, commands: &Receiver<Command>, output: &Mutex<Status>) {
    let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| machine.run(commands))) else {
        return;
    };
    let message = text::worker_panicked(panic_message(&*payload)).to_string();
    log::error(&message);
    // Stops capture and finishes a save if it can. A second panic is not handled further.
    let _ = panic::catch_unwind(AssertUnwindSafe(|| machine.shut_down()));
    let mut status = output.lock().unwrap_or_else(PoisonError::into_inner);
    status.state = State::Stopped;
    status.problem = Some(Problem {
        kind: ProblemKind::WorkerStopped,
        at: log::clock(),
        cause: message,
    });
}

/// Returns a panicking thread's message, when its payload is a string.
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
