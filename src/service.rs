//! The recording worker: a thread that keeps one capture session running, starts it again when
//! it ends on its own, and saves clips from its buffer. The window sends it commands, tells it
//! which full-screen app is in front, and reads its status once a second.

use crate::{
    audio,
    config::{Backend, Config, Mode, Theme},
    log,
    replay::{self, Clip, Live, SaveError, Stream},
    storage::{Store, Totals},
    sys::{self, sound::Tone},
};
use anyhow::Result;
use std::{
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

mod text;

/// The span the frame rate is measured over; a publish sooner than that keeps the last rate, so
/// the figure changes about every ten seconds.
const RATE_SPAN: Duration = Duration::from_secs(10);
/// The span of the first measurement after recording starts, so the figure appears promptly.
const FIRST_RATE_SPAN: Duration = Duration::from_secs(1);
/// How long to wait before starting a session again after it failed (the display was turned
/// off, the driver was updated, the GPU was reset).
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
/// How often the progress tone plays while a clip is being written.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// What a session captures. Handles are the OS's, carried as integers so the worker stays
/// portable; a stale one makes capture fail, which the worker treats like any other failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// A whole monitor; handle 0 is the primary one.
    Monitor { handle: isize, name: String },
    /// One app's window, and the monitor it is on, to fall back to.
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

    /// The window's monitor, for when the window itself cannot be captured.
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

/// Why a capture session ended without an error worth retrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It was asked to stop.
    Stopped,
    /// The captured display or window changed size; the encoder needs a new session.
    Resized,
    /// The captured window closed or the display went away.
    Closed,
    /// A window capture delivered no frame at all: the app presents in a way Windows cannot
    /// compose, so its monitor must be captured instead.
    NoFrames,
    /// This machine cannot record with these settings (no NVIDIA GPU, a driver too old, a
    /// codec the GPU lacks); trying again would fail the same way.
    Unsupported(String),
}

/// Runs a capture session of a target into `live` until `stop` is set or it fails.
pub(crate) type Recorder = fn(&Config, &Target, &Live, &AtomicBool) -> Result<Ended>;

/// The recorder of real sessions: the GPU pipeline.
#[cfg(windows)]
const RECORDER: Recorder = crate::gpu::record;
/// The recorder of real sessions: capture needs Windows, so every session fails.
#[cfg(not(windows))]
const RECORDER: Recorder = |_, _, _, _| anyhow::bail!("capture requires Windows");

/// Writes a clip into a directory and returns its path.
type Writer = fn(&Clip, &Path) -> Result<PathBuf, SaveError>;
/// Plays a tone about a save.
type Chime = Box<dyn Fn(Tone) + Send>;

/// What saving a clip goes through: the writer, on the save thread, and the tones, played from
/// the worker. Tests replace both, to make a save slow or fail and to hear the tones.
struct Saver {
    write: Writer,
    chime: Chime,
}

impl Saver {
    /// The saver of real clips: the MP4 writer and the sound card.
    fn real() -> Self {
        Self {
            write: replay::save,
            chime: Box::new(sys::sound::play),
        }
    }
}

/// What the worker is doing, for the Status tab to put into words.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) enum DisplayStatus {
    /// A session is starting and its encoder has not reported the stream yet.
    #[default]
    Starting,
    /// No full-screen app is in front.
    Waiting,
    /// A session is recording into the buffer.
    Recording,
    /// A session failed, and the worker waits before it tries again.
    Retrying,
    /// The user stopped recording, or a session ended because it was asked to.
    Stopped,
    /// These settings cannot record on this machine.
    Unsupported,
}

/// The worker's figures as the window reads them.
#[derive(Clone, Default, Debug)]
pub(crate) struct Status {
    pub(crate) state: DisplayStatus,
    pub(crate) recording: bool,
    /// What the current or latest session captures.
    pub(crate) source: Option<String>,
    /// The capture backend of the current or latest session.
    pub(crate) backend: Option<Backend>,
    pub(crate) stream: Option<Stream>,
    pub(crate) bitrate_mbps: u32,
    /// Bytes of the clips saved to disk in every run; `None` when the totals cannot be read or
    /// kept.
    pub(crate) saved_bytes: Option<u64>,
    /// Bytes that went into the replay buffer in every run, this session's included.
    pub(crate) buffered_lifetime_bytes: Option<u64>,
    /// Frames per second encoded, over the last refresh.
    pub(crate) encode_rate: Option<f64>,
    /// Bytes that went into the replay buffer since the app started.
    pub(crate) session_written_bytes: u64,
    /// Bytes of the clips saved since the app started.
    pub(crate) session_saved_bytes: u64,
    pub(crate) saving: bool,
    pub(crate) last_clip: Option<PathBuf>,
    pub(crate) error: Option<String>,
    pub(crate) notice: Option<String>,
}

#[derive(Debug)]
enum Command {
    Start(Config),
    /// A full-screen app is in front; recorded in that mode, unless it already is.
    Target(Target),
    Stop,
    Save,
    /// The window changed its look; remembered without touching the session.
    Theme(Theme),
    /// Sent by a capture or save thread that has something to report, and by the stream
    /// listener once the encoder is configured: the worker looks at its sessions and publishes.
    Wake,
    /// The window wants fresh figures for the Status tab.
    Publish,
    Exit,
}

/// The window's handle on the recording worker; dropping it stops the worker and waits for it.
pub(crate) struct Worker {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn spawn(store: Store, config: Config) -> Self {
        Self::spawn_with(RECORDER, RETRY_INTERVAL, Some(store), config)
    }

    fn spawn_with(
        recorder: Recorder,
        retry_interval: Duration,
        store: Option<Store>,
        config: Config,
    ) -> Self {
        Self::spawn_saving_with(recorder, retry_interval, store, config, Saver::real())
    }

    fn spawn_saving_with(
        recorder: Recorder,
        retry_interval: Duration,
        store: Option<Store>,
        config: Config,
        saver: Saver,
    ) -> Self {
        let (commands, receiver) = mpsc::channel();
        let wake = commands.clone();
        let status = Arc::new(Mutex::new(Status::default()));
        let output = Arc::clone(&status);
        let join = thread::Builder::new()
            .name("recording-worker".to_owned())
            .spawn(move || {
                let mut machine =
                    Machine::new(recorder, retry_interval, store, config, output, wake, saver);
                machine.run(&receiver);
            })
            .expect("spawn the recording worker thread");

        Self {
            commands,
            status,
            join: Some(join),
        }
    }

    pub(crate) fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Saves `config` and starts recording with it, replacing the current session.
    pub(crate) fn start(&self, config: Config) {
        self.request(Command::Start(config));
    }

    /// Tells the worker which full-screen app is in front.
    pub(crate) fn target(&self, target: Target) {
        self.request(Command::Target(target));
    }

    /// Stops recording until the settings are applied again.
    pub(crate) fn stop(&self) {
        self.request(Command::Stop);
    }

    /// Saves the replay buffer as a clip.
    pub(crate) fn save(&self) {
        self.request(Command::Save);
    }

    /// Saves the chosen look in the settings; recording carries on undisturbed.
    pub(crate) fn set_theme(&self, theme: Theme) {
        self.request(Command::Theme(theme));
    }

    /// Asks for fresh figures; they are read with [`Worker::status`] afterwards.
    pub(crate) fn refresh(&self) {
        self.request(Command::Publish);
    }

    fn request(&self, command: Command) {
        // The worker only goes away with this handle; a send cannot fail before then.
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

/// A running capture session.
struct Session {
    stop: Arc<AtomicBool>,
    join: JoinHandle<Result<Ended>>,
    /// The sound capture feeding the same buffer, when there is one; dropping the session
    /// stops it.
    _audio: Option<audio::Capture>,
}

/// A clip being written.
struct Saving {
    seconds: f64,
    join: JoinHandle<Result<PathBuf, SaveError>>,
    /// When the progress tone plays next.
    next_tick: Instant,
}

/// Counters at the previous refresh, for the rates.
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    encoded: u64,
}

/// The session and its buffer as one publish sees them.
struct Reading {
    running: bool,
    stream: Option<Stream>,
    /// Bytes in the current buffer, read once so the session and lifetime figures agree.
    live_bytes: u64,
    sample: Sample,
}

/// The worker thread's state: the settings, the session and its buffer, the clip being written
/// and the figures for the Status tab.
struct Machine {
    recorder: Recorder,
    retry_interval: Duration,
    store: Option<Store>,
    /// The lifetime totals, not counting the buffer of the latest session until it is banked;
    /// `None` when they cannot be read, so nothing is written over them.
    totals: Option<Totals>,
    /// Bytes that went into the buffers of the sessions banked so far, since the app started.
    session_buffered: u64,
    /// Bytes of the clips saved since the app started.
    session_saved: u64,
    config: Config,
    output: Arc<Mutex<Status>>,
    status: Status,
    session: Option<Session>,
    /// What the current or latest session captures.
    target: Option<Target>,
    /// The latest session's buffer; it stays saveable after the session fails.
    live: Option<Arc<Live>>,
    /// When a failed session is tried again (whole-desktop mode), or when targets are accepted
    /// again (full-screen mode).
    retry_at: Option<Instant>,
    /// Stopped by the user; targets are ignored until the settings are applied again.
    paused: bool,
    saving: Option<Saving>,
    sample: Option<Sample>,
    /// Lets the capture and save threads wake the worker when they finish or start streaming.
    wake: Sender<Command>,
    saver: Saver,
}

impl Machine {
    fn new(
        recorder: Recorder,
        retry_interval: Duration,
        store: Option<Store>,
        config: Config,
        output: Arc<Mutex<Status>>,
        wake: Sender<Command>,
        saver: Saver,
    ) -> Self {
        let totals = store.as_ref().and_then(load_totals);
        Self {
            recorder,
            retry_interval,
            store,
            totals,
            session_buffered: 0,
            session_saved: 0,
            config,
            output,
            status: Status::default(),
            session: None,
            target: None,
            live: None,
            retry_at: None,
            paused: false,
            saving: None,
            sample: None,
            wake,
            saver,
        }
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        self.begin();
        self.publish();
        loop {
            match self.receive(commands) {
                Ok(Command::Start(config)) => self.apply(config),
                Ok(Command::Target(target)) => self.offer(target),
                Ok(Command::Stop) => self.pause(),
                Ok(Command::Save) => self.save(),
                Ok(Command::Theme(theme)) => self.remember_theme(theme),
                Ok(Command::Wake | Command::Publish) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Command::Exit) | Err(RecvTimeoutError::Disconnected) => break,
            }
            self.poll();
            self.publish();
        }
        self.shut_down();
    }

    /// Waits for the next command. Nothing wakes the worker while it is idle: it sleeps until a
    /// command or a thread wakes it, until a failed session is due to be tried again, or until
    /// the progress tone of a clip being written is due.
    fn receive(&self, commands: &Receiver<Command>) -> Result<Command, RecvTimeoutError> {
        let tick = self.saving.as_ref().map(|saving| saving.next_tick);
        match self.retry_at.into_iter().chain(tick).min() {
            Some(due) => commands.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }

    /// Ends the session, finishes a clip being written and banks the buffer before the worker
    /// exits.
    fn shut_down(&mut self) {
        self.stop_session();
        // A clip being written is finished before the application goes away.
        if let Some(saving) = self.saving.take() {
            self.finish_save(saving);
        }
        self.bank_live();
        self.publish();
    }

    /// Writes `config` to the settings file and starts recording with it, replacing the current
    /// session. Settings that cannot be written are not applied.
    fn apply(&mut self, config: Config) {
        if let Err(message) = self.write_settings(&config) {
            self.report_error(message);
            return;
        }
        log::info(text::settings_applied(&config));
        self.config = config;
        self.stop_session();
        self.paused = false;
        self.begin();
    }

    /// A full-screen app is in front. In full-screen mode it becomes the target, unless it
    /// already is, the user has stopped recording, a failure is cooling down, or the settings
    /// cannot work on this machine.
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

    /// Stops recording at the user's request, until the settings are applied again.
    fn pause(&mut self) {
        self.stop_session();
        self.retry_at = None;
        self.paused = true;
        self.status.state = DisplayStatus::Stopped;
        log::info(text::RECORDING_STOPPED);
    }

    /// Starts writing the buffer to a clip, unless a clip is already being written or there is
    /// nothing to save. The first progress tone plays at once, so the user hears that the hotkey
    /// registered.
    fn save(&mut self) {
        if self.saving.is_some() {
            log::info(text::ALREADY_SAVING);
            return;
        }
        let Some(clip) = self.live.as_ref().and_then(|live| live.clip()) else {
            log::warning(text::NOTHING_TO_SAVE);
            self.status.notice = Some(text::NOTHING_TO_SAVE.to_owned());
            (self.saver.chime)(Tone::Failed);
            return;
        };

        let seconds = clip.seconds();
        match self.spawn_save(clip) {
            Ok(join) => {
                (self.saver.chime)(Tone::Progress);
                self.saving = Some(Saving {
                    seconds,
                    join,
                    next_tick: Instant::now() + PROGRESS_INTERVAL,
                });
                self.status.notice = None;
            }
            Err(error) => self.report_failed_save(text::save_failed(error)),
        }
    }

    /// Writes the settings with the new look to the settings file. The other settings stay as
    /// last applied, and the session is not restarted.
    fn remember_theme(&mut self, theme: Theme) {
        self.config.theme = theme;
        if let Err(message) = self.write_settings(&self.config) {
            self.report_error(message);
        }
    }

    /// Writes `config` to the settings file, when there is one; the error is the message to
    /// report.
    fn write_settings(&self, config: &Config) -> Result<(), String> {
        match &self.store {
            Some(store) => store
                .save_config(config)
                .map_err(text::save_settings_failed),
            None => Ok(()),
        }
    }

    /// Starts recording as the mode says: the primary monitor at once, or nothing until a
    /// full-screen app is reported.
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

    /// Banks the previous buffer and starts a capture session of `target` into a fresh one,
    /// with the sound when the settings ask for it.
    fn start_session(&mut self, target: Target) {
        self.bank_live();
        let live = Arc::new(Live::new(self.config.buffer_frames()));
        live.on_stream(self.waker());
        let stop = Arc::new(AtomicBool::new(false));
        // Tests record without sound, so they do not depend on the machine's audio devices.
        let audio = (self.config.audio && !cfg!(test))
            .then(|| audio::start(&live))
            .flatten();
        let spawned = self.spawn_capture(&target, &live, &stop);

        self.retry_at = None;
        self.sample = None;
        self.target = Some(target);
        match spawned {
            Ok(join) => {
                self.session = Some(Session {
                    stop,
                    join,
                    _audio: audio,
                });
                self.live = Some(live);
                self.status.state = DisplayStatus::Starting;
                self.status.error = None;
            }
            Err(error) => self.fail(&text::capture_thread_failed(error)),
        }
    }

    /// Starts a thread that records `target` into `live` until `stop` is set, and wakes the
    /// worker when the session ends.
    fn spawn_capture(
        &self,
        target: &Target,
        live: &Arc<Live>,
        stop: &Arc<AtomicBool>,
    ) -> io::Result<JoinHandle<Result<Ended>>> {
        let (recorder, config, target, live, stop) = (
            self.recorder,
            self.config.clone(),
            target.clone(),
            Arc::clone(live),
            Arc::clone(stop),
        );
        let wake = self.waker();
        thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || {
                let ended = recorder(&config, &target, &live, &stop);
                wake();
                ended
            })
    }

    /// A callback that wakes the worker, for the capture and save threads and the stream
    /// listener; once the worker is gone it does nothing.
    fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let wake = self.wake.clone();
        move || {
            let _ = wake.send(Command::Wake);
        }
    }

    /// Starts a capture session of the current target again, after a resize or a retry.
    fn restart_session(&mut self) {
        // `start_session` puts the target back.
        match self.target.take() {
            Some(target) => self.start_session(target),
            None => self.begin(),
        }
    }

    /// Stops the running capture session, if there is one, and waits for it; a session that
    /// failed on its way out is logged.
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

    /// Reports a failed session. Whole-desktop recording is tried again after a while; in
    /// full-screen mode the worker waits that long before it accepts a target again.
    fn fail(&mut self, error: &str) {
        self.report_error(text::session_failed(error));
        self.status.state = DisplayStatus::Retrying;
        self.retry_at = Some(Instant::now() + self.retry_interval);
    }

    /// Logs `message` and shows it on the Status tab.
    fn report_error(&mut self, message: String) {
        log::error(&message);
        self.status.error = Some(message);
    }

    /// Notices a session that ended by itself, a retry that is due, a save that finished and a
    /// progress tone that is due.
    fn poll(&mut self) {
        if let Some(session) = self.session.take_if(|session| session.join.is_finished()) {
            self.session_ended(session.join.join());
        }

        if self.retry_at.is_some_and(|due| Instant::now() >= due) {
            self.retry();
        }

        // A finished save is reported before the tick, so no blip plays after its outcome.
        if let Some(saving) = self.saving.take_if(|saving| saving.join.is_finished()) {
            self.finish_save(saving);
        }
        self.tick_save();
    }

    /// Acts on how a session that ended by itself finished.
    fn session_ended(&mut self, outcome: thread::Result<Result<Ended>>) {
        match outcome {
            Ok(Ok(Ended::Stopped)) => self.status.state = DisplayStatus::Stopped,
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
                self.report_error(text::session_failed(&reason));
                self.status.state = DisplayStatus::Unsupported;
            }
            Ok(Err(error)) => self.fail(&format!("{error:#}")),
            Err(_) => self.fail(text::CAPTURE_PANICKED),
        }
    }

    /// A window capture delivered no frame, so its monitor is captured instead; a monitor that
    /// delivers none is a failure.
    fn fall_back_to_monitor(&mut self) {
        let Some(window @ Target::Window { .. }) = &self.target else {
            self.fail(text::NO_FRAMES);
            return;
        };
        log::info(text::falling_back_to_monitor(window.name()));
        let monitor = window.monitor();
        self.start_session(monitor);
    }

    /// Ends the wait after a failure: whole-desktop recording starts again, and full-screen mode
    /// accepts targets again.
    fn retry(&mut self) {
        match self.config.mode {
            Mode::Desktop => {
                log::info(text::RESTARTING);
                self.restart_session();
            }
            // The window may be gone; the next report of a full-screen app starts again.
            Mode::Fullscreen => self.begin(),
        }
    }

    /// Starts a thread that writes `clip` to the clip directory, and wakes the worker when it is
    /// done.
    fn spawn_save(&self, clip: Clip) -> io::Result<JoinHandle<Result<PathBuf, SaveError>>> {
        let directory = self.config.clip_directory();
        let write = self.saver.write;
        let wake = self.waker();
        thread::Builder::new()
            .name("save".to_owned())
            .spawn(move || {
                if let Err(error) = sys::thread::enter_background_mode() {
                    log::warning(text::background_save_failed(error));
                }
                let saved = write(&clip, &directory);
                wake();
                saved
            })
    }

    /// Plays the progress tone while a clip is being written, once a second after the first.
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
        (self.saver.chime)(Tone::Progress);
    }

    /// Waits for a clip being written and reports how it went.
    fn finish_save(&mut self, saving: Saving) {
        match saving.join.join() {
            Ok(Ok(path)) => self.clip_saved(saving.seconds, path),
            Ok(Err(error)) => self.report_failed_save(text::save_failed(error)),
            Err(_) => self.report_failed_save(text::save_failed(text::SAVE_PANICKED)),
        }
    }

    /// Logs a clip written to `path`, counts it and plays the saved sound.
    fn clip_saved(&mut self, seconds: f64, path: PathBuf) {
        log::info(text::clip_saved(seconds, &path));
        self.add_saved(&path);
        self.status.last_clip = Some(path);
        self.status.error = None;
        (self.saver.chime)(Tone::Saved);
    }

    /// Reports a clip that could not be written and plays the failed sound.
    fn report_failed_save(&mut self, message: String) {
        self.report_error(message);
        (self.saver.chime)(Tone::Failed);
    }

    /// Adds a saved clip's size to the lifetime total and writes the totals to disk.
    fn add_saved(&mut self, path: &Path) {
        let bytes = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
        self.session_saved = self.session_saved.saturating_add(bytes);
        if let Some(totals) = self.totals.as_mut() {
            totals.saved_bytes = totals.saved_bytes.saturating_add(bytes);
        }
        self.persist_totals();
    }

    /// Adds the buffer of the latest session to the lifetime total and forgets the buffer.
    /// Called before a new session replaces it, and when the worker stops.
    fn bank_live(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        let bytes = live.bytes();
        self.session_buffered = self.session_buffered.saturating_add(bytes);
        if let Some(totals) = self.totals.as_mut() {
            totals.buffered_bytes = totals.buffered_bytes.saturating_add(bytes);
        }
        self.persist_totals();
    }

    /// Writes the totals to disk, if they were read; a failure is logged and the next write
    /// tries again.
    fn persist_totals(&self) {
        if let (Some(store), Some(totals)) = (&self.store, self.totals)
            && let Err(error) = store.save_totals(totals)
        {
            log::warning(text::save_totals_failed(error));
        }
    }

    /// Brings the status up to date and hands a copy to the window.
    fn publish(&mut self) {
        let reading = self.read_live();
        self.mark_recording(&reading);
        self.describe_session(&reading);
        self.count_bytes(reading.live_bytes);
        self.measure_rate(reading.running, reading.sample);
        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = self.status.clone();
    }

    /// Reads the session and its buffer once, so every figure of a publish agrees.
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
            sample: Sample {
                at: Instant::now(),
                encoded,
            },
        }
    }

    /// Logs a starting session as recording once its encoder has reported the stream.
    fn mark_recording(&mut self, reading: &Reading) {
        if let Some(stream) = &reading.stream
            && reading.running
            && self.status.state == DisplayStatus::Starting
        {
            log::info(text::recording(stream, &self.config));
            self.status.state = DisplayStatus::Recording;
        }
    }

    /// Fills in what is being recorded, how, and whether a clip is being written.
    fn describe_session(&mut self, reading: &Reading) {
        self.status.recording = reading.running;
        self.status.source = self.target.as_ref().map(|target| target.name().to_owned());
        self.status.backend = reading.running.then_some(self.config.backend);
        self.status.stream = reading.stream;
        self.status.bitrate_mbps = self.config.bitrate_mbps;
        self.status.saving = self.saving.is_some();
    }

    /// Fills in the bytes buffered and saved, since the app started and in every run, counting
    /// `live_bytes` in the current buffer.
    fn count_bytes(&mut self, live_bytes: u64) {
        self.status.session_written_bytes = self.session_buffered.saturating_add(live_bytes);
        self.status.session_saved_bytes = self.session_saved;
        self.status.saved_bytes = self.totals.map(|totals| totals.saved_bytes);
        self.status.buffered_lifetime_bytes = self
            .totals
            .map(|totals| totals.buffered_bytes.saturating_add(live_bytes));
    }

    /// Updates the encode rate from the counters at `current`. It is cleared while nothing
    /// records, and starts over with the next session.
    fn measure_rate(&mut self, running: bool, current: Sample) {
        let Some(previous) = self.sample.filter(|_| running) else {
            self.status.encode_rate = None;
            self.sample = running.then_some(current);
            return;
        };
        // A publish soon after the last one keeps its rate: the figure moves slowly.
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

/// The lifetime totals kept in `store`; unreadable ones are logged and not kept.
fn load_totals(store: &Store) -> Option<Totals> {
    store
        .load_totals()
        .inspect_err(|error| log::error(text::load_totals_failed(error)))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::Packet;
    use std::sync::atomic::AtomicUsize;

    /// Encodes a fake keyframe every 10 frames until stopped.
    #[expect(clippy::unnecessary_wraps)]
    fn steady(config: &Config, _: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
        live.set_stream(Stream {
            codec: config.codec,
            width: 64,
            height: 64,
            frame_rate: config.frame_rate,
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
                true,
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

    /// Sessions in order: the first window ends as if its app closed, a second window yields
    /// no frames, and a monitor records steadily.
    static SESSIONS: AtomicUsize = AtomicUsize::new(0);

    fn scripted(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
        SESSIONS.fetch_add(1, Ordering::Relaxed);
        match target {
            Target::Window { window: 1, .. } => {
                // Long enough for the status tick to see it recording.
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

    fn wait_for(worker: &Worker, done: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Like the window's status timer: figures that grow without an event need a request.
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
            clip_directory: directory.display().to_string(),
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
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, None, config_in(directory.path()));

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
    fn the_lifetime_totals_grow_and_survive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let clips = directory.path().join("clips");
        let config = config_in(&clips);
        let store = Store::at(state.clone()).unwrap();
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, Some(store), config.clone());
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
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, Some(store), config);
        let status = wait_for(&worker, |status| status.saved_bytes.is_some());
        assert_eq!(status.saved_bytes, Some(clip_bytes));
        assert!(status.buffered_lifetime_bytes.unwrap() >= buffered);
    }

    #[test]
    fn stopping_ends_the_session_and_its_buffer_stays_saveable() {
        let directory = tempfile::tempdir().unwrap();
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, None, config_in(directory.path()));
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.stop();
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Stopped);
        assert!(!status.recording);

        // The stopped session's buffer can still be saved.
        worker.save();
        wait_for(&worker, |status| status.last_clip.is_some());
    }

    #[test]
    fn a_failed_session_is_retried() {
        let worker = Worker::spawn_with(failing, Duration::from_millis(50), None, desktop());

        let status = wait_for(&worker, |status| status.state == DisplayStatus::Retrying);
        assert!(status.error.unwrap().contains("no capture device"));

        worker.save();
        let status = wait_for(&worker, |status| status.notice.is_some());
        assert_eq!(status.notice.as_deref(), Some(text::NOTHING_TO_SAVE));
    }

    #[test]
    fn an_unsupported_configuration_is_not_retried() {
        let worker = Worker::spawn_with(unsupported, Duration::from_millis(10), None, desktop());

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
        let worker = Worker::spawn_with(scripted, RETRY_INTERVAL, None, config);

        let status = wait_for(&worker, |status| status.state == DisplayStatus::Waiting);
        assert!(!status.recording);
        assert!(status.source.is_none());

        // The app comes to the front; reporting it again does not restart the session.
        worker.target(window(1));
        wait_for(&worker, |status| status.state == DisplayStatus::Recording);
        worker.target(window(1));
        worker.target(window(1));
        // Its session ends as if it closed; the buffer stays saveable.
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Waiting);
        assert_eq!(SESSIONS.load(Ordering::Relaxed), 1);
        assert_eq!(status.source.as_deref(), Some("game1.exe"));
        worker.save();
        wait_for(&worker, |status| status.last_clip.is_some());

        // A second app cannot be captured as a window, so its monitor is captured instead.
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
        let worker = Worker::spawn_with(failing, Duration::from_millis(100), None, config);
        wait_for(&worker, |status| status.state == DisplayStatus::Waiting);

        worker.target(window(1));
        let status = wait_for(&worker, |status| status.state == DisplayStatus::Retrying);
        assert!(status.error.is_some());
        // During the cooldown the same app is not tried again; afterwards it is.
        worker.target(window(1));
        thread::sleep(Duration::from_millis(20));
        assert_eq!(worker.status().state, DisplayStatus::Retrying);
        wait_for(&worker, |status| status.state == DisplayStatus::Waiting);

        worker.stop();
        wait_for(&worker, |status| status.state == DisplayStatus::Stopped);
        worker.target(window(1));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(worker.status().state, DisplayStatus::Stopped);
    }

    #[test]
    fn applying_settings_persists_them_and_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("state")).unwrap();
        let worker = Worker::spawn_with(
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
        let worker = Worker::spawn_with(
            steady,
            RETRY_INTERVAL,
            Some(Store::at(state.clone()).unwrap()),
            config.clone(),
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.set_theme(Theme::Nova);
        // Commands are handled in order, so by the time the worker has exited it has saved.
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

    /// The tones a test saver played, with when.
    type Heard = Arc<Mutex<Vec<(Instant, Tone)>>>;

    /// A saver with the writer `write` whose tones are written down instead of played.
    fn listening(write: Writer) -> (Saver, Heard) {
        let heard = Heard::default();
        let ear = Arc::clone(&heard);
        let chime = Box::new(move |tone| {
            ear.lock().unwrap().push((Instant::now(), tone));
        });
        (Saver { write, chime }, heard)
    }

    fn tones(heard: &Heard) -> Vec<Tone> {
        heard
            .lock()
            .unwrap()
            .iter()
            .map(|&(_, tone)| tone)
            .collect()
    }

    /// Asserts that `heard` is progress blips at least most of a second apart, then `outcome`
    /// once, and returns the number of blips.
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

    /// Writes the clip after two and a half seconds.
    fn slow_write(clip: &Clip, directory: &Path) -> Result<PathBuf, SaveError> {
        thread::sleep(Duration::from_millis(2500));
        replay::save(clip, directory)
    }

    /// Fails after a second and a half.
    fn failing_write(_: &Clip, directory: &Path) -> Result<PathBuf, SaveError> {
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
        let (saver, heard) = listening(slow_write);
        let worker = Worker::spawn_saving_with(
            steady,
            RETRY_INTERVAL,
            None,
            config_in(directory.path()),
            saver,
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.save();
        wait_for(&worker, |status| status.saving);
        assert_eq!(tones(&heard), [Tone::Progress], "the first blip is at once");
        wait_for(&worker, |status| status.last_clip.is_some());
        let blips = assert_ticks_then(&heard, Tone::Saved);
        assert!((2..=3).contains(&blips), "{blips} blips in 2.5 seconds");

        // The ticking stopped with the save.
        thread::sleep(PROGRESS_INTERVAL + Duration::from_millis(300));
        assert_eq!(tones(&heard).len(), blips + 1);
    }

    #[test]
    fn a_failed_save_sounds_failed_and_stops_ticking() {
        let directory = tempfile::tempdir().unwrap();
        let (saver, heard) = listening(failing_write);
        let worker = Worker::spawn_saving_with(
            steady,
            RETRY_INTERVAL,
            None,
            config_in(directory.path()),
            saver,
        );
        wait_for(&worker, |status| status.session_written_bytes > 0);

        worker.save();
        let status = wait_for(&worker, |status| status.error.is_some() && !status.saving);
        assert!(status.error.unwrap().contains("the disk is full"));
        assert!(status.last_clip.is_none());
        let blips = assert_ticks_then(&heard, Tone::Failed);
        assert!((1..=2).contains(&blips), "{blips} blips in 1.5 seconds");

        thread::sleep(PROGRESS_INTERVAL + Duration::from_millis(300));
        assert_eq!(tones(&heard).len(), blips + 1);
    }

    #[test]
    fn nothing_to_save_sounds_failed_without_ticking() {
        let (saver, heard) = listening(slow_write);
        let worker =
            Worker::spawn_saving_with(failing, Duration::from_secs(60), None, desktop(), saver);
        wait_for(&worker, |status| status.state == DisplayStatus::Retrying);

        worker.save();
        wait_for(&worker, |status| status.notice.is_some());
        assert_eq!(tones(&heard), [Tone::Failed]);
    }
}
