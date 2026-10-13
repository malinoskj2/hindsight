//! The worker's tests, driving it through stand-in services.

use super::*;
use crate::{
    audio::EncodeError,
    replay::{Matrix, Packet, Picture},
};
use std::sync::atomic::AtomicUsize;

#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps the session function's signature"
)]
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

#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps the session function's signature"
)]
fn unsupported(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
    Ok(Ended::Unsupported(
        "this GPU's NVENC has no AV1 encoder".to_owned(),
    ))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps the session function's signature"
)]
fn closing(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
    Ok(Ended::Closed)
}

static SESSIONS: AtomicUsize = AtomicUsize::new(0);
static FAILED_SESSIONS: AtomicUsize = AtomicUsize::new(0);

// Each stand-in below runs until its test lets it go; one test uses each.
static GAME_CLOSES: AtomicBool = AtomicBool::new(false);
static SLOW_WRITE_ENDS: AtomicBool = AtomicBool::new(false);
static FAILING_WRITE_ENDS: AtomicBool = AtomicBool::new(false);

fn hold_until(gate: &AtomicBool, stop: &AtomicBool) {
    while !gate.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
    }
}

fn failing_counted(
    config: &Config,
    target: &Target,
    live: &Live,
    stop: &AtomicBool,
) -> Result<Ended> {
    FAILED_SESSIONS.fetch_add(1, Ordering::Relaxed);
    failing(config, target, live, stop)
}

fn scripted(config: &Config, target: &Target, live: &Live, stop: &AtomicBool) -> Result<Ended> {
    SESSIONS.fetch_add(1, Ordering::Relaxed);
    match target {
        Target::Window { window, .. } if window.get() == 1 => {
            steady_until_closed(config, live, stop);
            Ok(Ended::Closed)
        }
        Target::Window { .. } => Ok(Ended::NoFrames),
        Target::Monitor { .. } => steady(config, target, live, stop),
    }
}

fn steady_until_closed(config: &Config, live: &Live, stop: &AtomicBool) {
    let ended = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            hold_until(&GAME_CLOSES, stop);
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
        clock: Box::new(Instant::now),
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
    poll(worker, true, done)
}

/// Like `wait_for`, but never wakes the worker itself: what is awaited must.
fn wait_unprompted(worker: &Worker, done: impl Fn(&Status) -> bool) -> Status {
    poll(worker, false, done)
}

fn poll(worker: &Worker, prompt: bool, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if prompt {
            worker.refresh();
        }
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

/// A clock that moves only when the test moves it.
struct ManualClock(Arc<Mutex<Instant>>);

impl ManualClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    fn reader(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Box::new(move || *now.lock().unwrap())
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
        clip_directory: Some(directory.to_path_buf()),
        ..desktop()
    }
}

fn window(window: isize) -> Target {
    Target::Window {
        window: WindowHandle::new(window).unwrap(),
        monitor: MonitorHandle::new(7).unwrap(),
        name: format!("game{window}.exe"),
    }
}

#[test]
fn desktop_mode_records_and_saves_a_clip() {
    let directory = tempfile::tempdir().unwrap();
    let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));

    let status = wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_)) && status.bytes.session_written > 0
    });
    assert!(status.state.running().is_some());
    assert_eq!(status.buffer.unwrap().stream.unwrap().frame_rate, 60);
    assert_eq!(status.source.as_deref(), Some("the primary monitor"));
    let config = desktop();
    assert_eq!(
        (
            status.bitrate_mbps,
            status.buffer_seconds,
            status.buffer_capacity_bytes
        ),
        (
            config.bitrate_mbps,
            config.buffer_seconds,
            config.estimated_buffer_bytes()
        )
    );

    worker.save();
    let status = wait_for(&worker, |status| status.last_clip.is_some());
    let clip = status.last_clip.unwrap();
    assert!(clip.starts_with(directory.path()));
    assert_eq!(&std::fs::read(&clip).unwrap()[4..8], b"ftyp");
}

#[test]
fn a_thread_that_ends_wakes_the_worker_without_a_refresh() {
    let worker = spawn(closing, RETRY_INTERVAL, None, desktop());
    let status = wait_unprompted(&worker, |status| status.state == State::Retrying);
    assert!(status.error.unwrap().contains(text::DISPLAY_GONE));

    let directory = tempfile::tempdir().unwrap();
    let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));
    wait_for(&worker, |status| status.bytes.session_written > 0);
    worker.save();
    wait_unprompted(&worker, |status| status.last_clip.is_some());
}

#[test]
fn the_lifetime_totals_grow_and_survive_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let clips = directory.path().join("clips");
    let config = config_in(&clips);
    let store = Store::at(state.clone()).unwrap();
    let worker = spawn(steady, RETRY_INTERVAL, Some(store), config.clone());
    let status = wait_for(&worker, |status| status.bytes.lifetime_saved.is_some());
    assert_eq!(status.bytes.lifetime_saved, Some(0));
    // A save before the first packet would find nothing to save.
    wait_for(&worker, |status| status.bytes.session_written > 0);

    worker.save();
    let status = wait_for(&worker, |status| status.last_clip.is_some());
    let clip_bytes = std::fs::metadata(status.last_clip.unwrap()).unwrap().len();
    assert_eq!(status.bytes.lifetime_saved, Some(clip_bytes));
    let buffered = status.bytes.lifetime_written.unwrap();
    assert!(buffered > 0, "the recording went into the buffer");
    drop(worker);

    let store = Store::at(state).unwrap();
    let worker = spawn(steady, RETRY_INTERVAL, Some(store), config);
    let status = wait_for(&worker, |status| status.bytes.lifetime_saved.is_some());
    assert_eq!(status.bytes.lifetime_saved, Some(clip_bytes));
    assert!(status.bytes.lifetime_written.unwrap() >= buffered);
}

#[test]
fn pausing_keeps_the_buffer_saveable_and_resuming_records_again() {
    let directory = tempfile::tempdir().unwrap();
    let worker = spawn(steady, RETRY_INTERVAL, None, config_in(directory.path()));
    wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_)) && status.bytes.session_written > 0
    });

    worker.toggle_pause();
    let status = wait_for(&worker, |status| status.state == State::Paused);
    assert!(status.state.running().is_none());
    worker.save();
    let status = wait_for(&worker, |status| status.last_clip.is_some());
    assert_eq!(status.state, State::Paused);

    worker.toggle_pause();
    wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_))
    });
    let before = worker.status().bytes.session_written;
    worker.restart();
    wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_)) && status.bytes.session_written > before
    });
}

#[test]
fn a_failed_session_is_retried() {
    let worker = spawn(failing, Duration::from_millis(50), None, desktop());

    let status = wait_for(&worker, |status| status.state == State::Retrying);
    assert!(status.error.unwrap().contains("no capture device"));

    worker.save();
    wait_for(&worker, |status| status.nothing_to_save);
}

#[test]
fn an_unsupported_configuration_is_not_retried() {
    let worker = spawn(unsupported, Duration::from_millis(10), None, desktop());

    let status = wait_for(&worker, |status| status.state == State::Unsupported);
    assert!(status.error.unwrap().contains("AV1"));
    assert!(status.state.running().is_none());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(worker.status().state, State::Unsupported);
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

    let status = wait_for(&worker, |status| status.state == State::Waiting);
    assert!(status.state.running().is_none());
    assert!(status.source.is_none());

    worker.target(window(1));
    wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_)) && status.bytes.session_written > 0
    });
    worker.target(window(1));
    worker.target(window(1));
    GAME_CLOSES.store(true, Ordering::Relaxed);
    let status = wait_for(&worker, |status| status.state == State::Waiting);
    assert_eq!(SESSIONS.load(Ordering::Relaxed), 1);
    assert_eq!(status.source.as_deref(), Some("game1.exe"));
    worker.save();
    wait_for(&worker, |status| status.last_clip.is_some());

    worker.target(window(2));
    let status = wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_)) && status.bytes.session_written > 0
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
    let clock = ManualClock::new();
    let worker = Worker::spawn_with(
        Services {
            clock: clock.reader(),
            ..services(failing_counted, RETRY_INTERVAL)
        },
        None,
        config,
    )
    .unwrap();
    wait_for(&worker, |status| status.state == State::Waiting);

    worker.target(window(1));
    let status = wait_for(&worker, |status| status.state == State::Retrying);
    assert!(status.error.is_some());
    worker.target(window(1));
    // Handled in order, so the save shows the target was seen during the cool-down.
    worker.save();
    let status = wait_for(&worker, |status| status.nothing_to_save);
    assert_eq!(status.state, State::Retrying);
    assert_eq!(FAILED_SESSIONS.load(Ordering::Relaxed), 1);
    clock.advance(RETRY_INTERVAL);
    wait_for(&worker, |status| status.state == State::Waiting);
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
    wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_))
    });

    let config = Config {
        frame_rate: 30,
        ..config_in(directory.path())
    };
    worker.start(config.clone());
    wait_for(&worker, |status| {
        status
            .buffer
            .and_then(|buffer| buffer.stream)
            .is_some_and(|stream| stream.frame_rate == 30)
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
    wait_for(&worker, |status| status.bytes.session_written > 0);

    worker.set_theme(Theme::Nova);
    let status = wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_))
    });
    assert!(status.state.running().is_some(), "the session carries on");
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

type Heard = Arc<Mutex<Vec<Tone>>>;

/// The worker runs on `clock`; its tones are heard in order.
fn spawn_listening(
    record: Recorder,
    clock: &ManualClock,
    write: Writer,
    config: Config,
) -> (Worker, Heard) {
    let heard = Heard::default();
    let ear = Arc::clone(&heard);
    let services = Services {
        write,
        chime: Box::new(move |tone| ear.lock().unwrap().push(tone)),
        clock: clock.reader(),
        ..services(record, Duration::from_secs(60))
    };
    (Worker::spawn_with(services, None, config).unwrap(), heard)
}

fn tones(heard: &Heard) -> Vec<Tone> {
    heard.lock().unwrap().clone()
}

/// Lets a second pass on `clock` for each further blip expected, waiting for each to sound.
fn tick(worker: &Worker, clock: &ManualClock, heard: &Heard, blips: usize) {
    for heard_so_far in 2..=blips {
        clock.advance(PROGRESS_INTERVAL);
        wait_for(worker, |_| tones(heard).len() == heard_so_far);
    }
}

/// Lets time pass after the outcome; no tone may follow it.
fn assert_silent_after(worker: &Worker, clock: &ManualClock, heard: &Heard) {
    let before = tones(heard).len();
    clock.advance(2 * PROGRESS_INTERVAL);
    worker.refresh();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(tones(heard).len(), before, "{:?}", tones(heard));
}

fn slow_write(snapshot: &Snapshot, directory: &Path) -> Result<Saved, SaveError> {
    hold_until(&SLOW_WRITE_ENDS, &AtomicBool::new(false));
    replay::save(snapshot, directory)
}

/// Saves the video as `replay::save` does, but as if the buffered sound could not be encoded.
fn silent_write(snapshot: &Snapshot, directory: &Path) -> Result<Saved, SaveError> {
    #[cfg(windows)]
    let error = EncodeError::Short { made: 0, needed: 1 };
    #[cfg(not(windows))]
    let error = EncodeError::Unsupported;
    Ok(Saved {
        sound_error: Some(error),
        ..replay::save(snapshot, directory)?
    })
}

fn failing_write(_: &Snapshot, directory: &Path) -> Result<Saved, SaveError> {
    hold_until(&FAILING_WRITE_ENDS, &AtomicBool::new(false));
    Err(SaveError::Io {
        step: "create",
        path: directory.to_owned(),
        source: io::Error::other("the disk is full"),
    })
}

#[test]
fn a_slow_save_ticks_each_second_then_sounds_saved_once() {
    let directory = tempfile::tempdir().unwrap();
    let clock = ManualClock::new();
    let (worker, heard) = spawn_listening(steady, &clock, slow_write, config_in(directory.path()));
    wait_for(&worker, |status| status.bytes.session_written > 0);

    worker.save();
    wait_for(&worker, |_| !tones(&heard).is_empty());
    assert_eq!(tones(&heard), [Tone::Progress], "the first blip is at once");
    tick(&worker, &clock, &heard, 3);
    SLOW_WRITE_ENDS.store(true, Ordering::Relaxed);
    let status = wait_for(&worker, |status| status.last_clip.is_some());
    assert!(!status.clip_without_sound);
    assert_eq!(
        tones(&heard),
        [Tone::Progress, Tone::Progress, Tone::Progress, Tone::Saved]
    );
    assert_silent_after(&worker, &clock, &heard);
}

#[test]
fn a_failed_save_sounds_failed_and_stops_ticking() {
    let directory = tempfile::tempdir().unwrap();
    let clock = ManualClock::new();
    let (worker, heard) =
        spawn_listening(steady, &clock, failing_write, config_in(directory.path()));
    wait_for(&worker, |status| status.bytes.session_written > 0);

    worker.save();
    wait_for(&worker, |_| !tones(&heard).is_empty());
    tick(&worker, &clock, &heard, 2);
    FAILING_WRITE_ENDS.store(true, Ordering::Relaxed);
    let status = wait_for(&worker, |status| status.error.is_some());
    assert!(status.error.unwrap().contains("the disk is full"));
    assert!(status.last_clip.is_none());
    assert_eq!(
        tones(&heard),
        [Tone::Progress, Tone::Progress, Tone::Failed]
    );
    assert_silent_after(&worker, &clock, &heard);
}

#[test]
fn nothing_to_save_sounds_failed_without_ticking() {
    let clock = ManualClock::new();
    let (worker, heard) = spawn_listening(failing, &clock, slow_write, desktop());
    wait_for(&worker, |status| status.state == State::Retrying);

    worker.save();
    wait_for(&worker, |status| status.nothing_to_save);
    assert_eq!(tones(&heard), [Tone::Failed]);
}

fn panicking(_: &Config, _: &Target, _: &Live, _: &AtomicBool) -> Result<Ended> {
    panic!("the GPU melted")
}

#[test]
fn a_capture_panic_is_reported_with_its_message_and_retried() {
    let worker = spawn(panicking, RETRY_INTERVAL, None, desktop());
    let status = wait_for(&worker, |status| status.state == State::Retrying);
    assert!(status.error.unwrap().contains("the GPU melted"));
}

#[test]
fn a_worker_that_panics_shows_it_has_stopped() {
    let services = Services {
        chime: Box::new(|_| panic!("the speaker caught fire")),
        ..services(failing, Duration::from_secs(60))
    };
    let worker = Worker::spawn_with(services, None, desktop()).unwrap();
    wait_for(&worker, |status| status.state == State::Retrying);

    worker.save();
    let status = wait_for(&worker, |status| status.state == State::Stopped);
    assert!(status.error.unwrap().contains("the speaker caught fire"));
    worker.save();
    assert_eq!(worker.status().state, State::Stopped);
}

#[test]
fn sound_that_could_not_start_is_reported_only_when_asked_for() {
    let worker = spawn(steady, RETRY_INTERVAL, None, desktop());
    let status = wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_))
    });
    assert_eq!(
        status.sound,
        Some(Sound::Failed),
        "the stand-in starts no sound"
    );

    let quiet = Config {
        audio: false,
        ..desktop()
    };
    let worker = spawn(steady, RETRY_INTERVAL, None, quiet);
    let status = wait_for(&worker, |status| {
        matches!(status.state, State::Recording(_))
    });
    assert_eq!(status.sound, None);
}

#[test]
fn a_target_is_known_by_its_handles_not_its_name() {
    let renamed = Target::Window {
        window: WindowHandle::new(1).unwrap(),
        monitor: MonitorHandle::new(7).unwrap(),
        name: "a full-screen app".to_owned(),
    };
    assert_eq!(renamed, window(1));
    assert_ne!(window(2), window(1));
    assert_ne!(window(1).monitor(), window(1));
    assert_eq!(window(1).monitor(), window(2).monitor());
    assert_ne!(window(1).monitor(), Target::primary_monitor());
    assert_eq!(MonitorHandle::new(7).map(MonitorHandle::get), Some(7));
    assert_eq!(MonitorHandle::new(0), None);
}

#[test]
fn a_clip_saved_without_its_sound_says_so_on_the_status() {
    let directory = tempfile::tempdir().unwrap();
    let clock = ManualClock::new();
    let (worker, heard) =
        spawn_listening(steady, &clock, silent_write, config_in(directory.path()));
    wait_for(&worker, |status| status.bytes.session_written > 0);

    worker.save();
    let status = wait_for(&worker, |status| status.last_clip.is_some());
    assert!(status.clip_without_sound);
    assert!(status.error.is_none(), "{:?}", status.error);
    assert_eq!(tones(&heard).last(), Some(&Tone::Saved));
}
