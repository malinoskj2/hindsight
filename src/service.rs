//! The recording worker: a thread that keeps one capture session running, starts it again when
//! it ends on its own, and saves clips from its buffer. The window sends it commands, tells it
//! which full-screen app is in front, and reads its status once a second.

use crate::{
    config::{Config, Mode},
    log,
    replay::{self, Live, SaveError, Stream},
    storage::Store,
    sys,
};
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod text;

/// How often the worker refreshes its status while no command arrives.
const TICK: Duration = Duration::from_secs(1);
/// How long to wait before starting a session again after it failed (the display was turned
/// off, the driver was updated, the GPU was reset).
const RETRY_INTERVAL: Duration = Duration::from_secs(10);

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

#[cfg(windows)]
const RECORDER: Recorder = crate::gpu::record;
#[cfg(not(windows))]
const RECORDER: Recorder = |_, _, _, _| anyhow::bail!("capture requires Windows");

/// What the worker is doing, for the Status tab to put into words.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) enum DisplayStatus {
    #[default]
    Starting,
    /// No full-screen app is in front.
    Waiting,
    Recording,
    Retrying,
    Stopped,
    Unsupported,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Status {
    pub(crate) state: DisplayStatus,
    pub(crate) recording: bool,
    /// What the current or latest session captures.
    pub(crate) source: Option<String>,
    pub(crate) stream: Option<Stream>,
    pub(crate) bitrate_mbps: u32,
    /// Frames per second with new screen content, over the last refresh.
    pub(crate) fresh_rate: Option<f64>,
    /// Frames per second encoded, over the last refresh.
    pub(crate) encode_rate: Option<f64>,
    pub(crate) buffered_seconds: f64,
    pub(crate) buffered_bytes: u64,
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
    Exit,
}

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
        let (commands, receiver) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let output = Arc::clone(&status);
        let join = thread::Builder::new()
            .name("recording-worker".to_owned())
            .spawn(move || {
                let mut machine = Machine::new(recorder, retry_interval, store, config, output);
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

    pub(crate) fn stop(&self) {
        self.request(Command::Stop);
    }

    pub(crate) fn save(&self) {
        self.request(Command::Save);
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
}

/// A clip being written.
struct Saving {
    seconds: f64,
    join: JoinHandle<Result<PathBuf, SaveError>>,
}

/// Counters at the previous refresh, for the rates.
struct Sample {
    at: Instant,
    encoded: u64,
    fresh: u64,
}

struct Machine {
    recorder: Recorder,
    retry_interval: Duration,
    store: Option<Store>,
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
}

impl Machine {
    fn new(
        recorder: Recorder,
        retry_interval: Duration,
        store: Option<Store>,
        config: Config,
        output: Arc<Mutex<Status>>,
    ) -> Self {
        Self {
            recorder,
            retry_interval,
            store,
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
        }
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        self.begin();
        self.publish();
        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Start(config)) => self.apply(config),
                Ok(Command::Target(target)) => self.offer(target),
                Ok(Command::Stop) => {
                    self.stop_session();
                    self.retry_at = None;
                    self.paused = true;
                    self.status.state = DisplayStatus::Stopped;
                    log::info(text::RECORDING_STOPPED);
                }
                Ok(Command::Save) => self.save(),
                Ok(Command::Exit) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll();
            self.publish();
        }

        self.stop_session();
        // A clip being written is finished before the application goes away.
        if let Some(saving) = self.saving.take() {
            self.finish_save(saving);
        }
        self.publish();
    }

    fn apply(&mut self, config: Config) {
        if let Some(store) = &self.store
            && let Err(error) = store.save_config(&config)
        {
            self.report_error(text::save_settings_failed(error));
            return;
        }
        log::info(text::settings_applied(&config));
        self.config = config;
        self.stop_session();
        self.paused = false;
        self.begin();
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

    fn start_session(&mut self, target: Target) {
        let live = Arc::new(Live::new(self.config.buffer_frames()));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorder, config, session_target, session_live, session_stop) = (
            self.recorder,
            self.config.clone(),
            target.clone(),
            Arc::clone(&live),
            Arc::clone(&stop),
        );
        let spawned = thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || recorder(&config, &session_target, &session_live, &session_stop));

        self.retry_at = None;
        self.sample = None;
        self.target = Some(target);
        match spawned {
            Ok(join) => {
                self.session = Some(Session { stop, join });
                self.live = Some(live);
                self.status.state = DisplayStatus::Starting;
                self.status.error = None;
            }
            Err(error) => self.fail(&text::capture_thread_failed(error)),
        }
    }

    /// Starts the current target again, after a resize or a retry.
    fn restart_session(&mut self) {
        match self.target.clone() {
            Some(target) => self.start_session(target),
            None => self.begin(),
        }
    }

    fn stop_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop.store(true, Ordering::Relaxed);
            match session.join.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => log::error(text::session_failed(&format!("{error:#}"))),
                Err(_) => log::error(text::session_failed(text::CAPTURE_PANICKED)),
            }
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

    /// Notices a session that ended by itself, a retry that is due and a save that finished.
    fn poll(&mut self) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.join.is_finished())
            && let Some(session) = self.session.take()
        {
            match session.join.join() {
                Ok(Ok(Ended::Resized)) => {
                    log::info(text::RESIZED);
                    self.restart_session();
                }
                Ok(Ok(Ended::Stopped)) => {
                    self.status.state = DisplayStatus::Stopped;
                }
                Ok(Ok(Ended::Closed)) => match self.config.mode {
                    Mode::Fullscreen => {
                        let name = self.target.as_ref().map_or("", Target::name).to_owned();
                        log::info(text::target_closed(&name));
                        self.status.state = DisplayStatus::Waiting;
                    }
                    Mode::Desktop => self.fail(text::DISPLAY_GONE),
                },
                Ok(Ok(Ended::NoFrames)) => match self.target.clone() {
                    Some(target @ Target::Window { .. }) => {
                        log::info(text::falling_back_to_monitor(target.name()));
                        self.start_session(target.monitor());
                    }
                    _ => self.fail(text::NO_FRAMES),
                },
                Ok(Ok(Ended::Unsupported(reason))) => {
                    self.report_error(text::session_failed(&reason));
                    self.status.state = DisplayStatus::Unsupported;
                }
                Ok(Err(error)) => self.fail(&format!("{error:#}")),
                Err(_) => self.fail(text::CAPTURE_PANICKED),
            }
        }

        if self.retry_at.is_some_and(|due| Instant::now() >= due) {
            match self.config.mode {
                Mode::Desktop => {
                    log::info(text::RESTARTING);
                    self.restart_session();
                }
                // The window may be gone; the next report of a full-screen app starts again.
                Mode::Fullscreen => self.begin(),
            }
        }

        if self
            .saving
            .as_ref()
            .is_some_and(|saving| saving.join.is_finished())
            && let Some(saving) = self.saving.take()
        {
            self.finish_save(saving);
        }
    }

    fn save(&mut self) {
        if self.saving.is_some() {
            log::info(text::ALREADY_SAVING);
            return;
        }
        let Some(clip) = self.live.as_ref().and_then(|live| live.clip()) else {
            log::warning(text::NOTHING_TO_SAVE);
            self.status.notice = Some(text::NOTHING_TO_SAVE.to_owned());
            return;
        };

        let seconds = clip.seconds();
        let directory = self.config.clip_directory();
        let spawned = thread::Builder::new()
            .name("save".to_owned())
            .spawn(move || {
                if let Err(error) = sys::thread::enter_background_mode() {
                    log::warning(text::background_save_failed(error));
                }
                replay::save(&clip, &directory)
            });
        match spawned {
            Ok(join) => {
                self.saving = Some(Saving { seconds, join });
                self.status.notice = None;
            }
            Err(error) => self.report_error(text::save_failed(error)),
        }
    }

    fn finish_save(&mut self, saving: Saving) {
        match saving.join.join() {
            Ok(Ok(path)) => {
                log::info(text::saved(saving.seconds, &path));
                self.status.last_clip = Some(path);
                self.status.error = None;
            }
            Ok(Err(error)) => self.report_error(text::save_failed(error)),
            Err(_) => self.report_error(text::save_failed(text::SAVE_PANICKED)),
        }
    }

    fn publish(&mut self) {
        let live = self.live.clone();
        let running = self.session.is_some();
        let stream = live.as_ref().and_then(|live| live.stream());

        if running && stream.is_some() && self.status.state == DisplayStatus::Starting {
            log::info(text::recording(stream.as_ref(), &self.config));
            self.status.state = DisplayStatus::Recording;
        }
        self.status.recording = running;
        self.status.source = self.target.as_ref().map(|target| target.name().to_owned());
        self.status.stream = stream;
        self.status.bitrate_mbps = self.config.bitrate_mbps;
        self.status.saving = self.saving.is_some();

        let (frames, bytes) = live.as_ref().map_or((0, 0), |live| live.buffered());
        self.status.buffered_seconds = frames as f64 / f64::from(self.config.frame_rate);
        self.status.buffered_bytes = bytes as u64;

        let now = Instant::now();
        let (encoded, fresh) = live.as_ref().map_or((0, 0), |live| live.counters());
        (self.status.encode_rate, self.status.fresh_rate) = match (&self.sample, running) {
            (Some(previous), true) => {
                let elapsed = now.duration_since(previous.at).as_secs_f64();
                if elapsed > 0.0 {
                    (
                        Some((encoded - previous.encoded) as f64 / elapsed),
                        Some((fresh - previous.fresh) as f64 / elapsed),
                    )
                } else {
                    (self.status.encode_rate, self.status.fresh_rate)
                }
            }
            _ => (None, None),
        };
        self.sample = running.then_some(Sample {
            at: now,
            encoded,
            fresh,
        });

        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = self.status.clone();
    }
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

    fn config_in(directory: &std::path::Path) -> Config {
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
    fn records_and_saves_a_clip() {
        let directory = tempfile::tempdir().unwrap();
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, None, config_in(directory.path()));

        let status = wait_for(&worker, |status| {
            status.state == DisplayStatus::Recording && status.buffered_bytes > 0
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
    fn stopping_ends_the_session_and_saving_then_reports_nothing_new() {
        let directory = tempfile::tempdir().unwrap();
        let worker = Worker::spawn_with(steady, RETRY_INTERVAL, None, config_in(directory.path()));
        wait_for(&worker, |status| status.buffered_bytes > 0);

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
            status.state == DisplayStatus::Recording && status.buffered_bytes > 0
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
}
