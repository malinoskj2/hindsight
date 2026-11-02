//! The trim view's player: a thread that decodes the clip, paces its pictures while it plays,
//! and posts each to the view as it is due.

use crate::{
    gpu::{Frame, Preview},
    log,
    replay::CLOCK_RATE,
    trim::Timeline,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use winsafe::{self as w, co};

/// Posted to the view when the player has a picture or an event for it.
#[expect(unsafe_code, reason = "builds a WM constant from a raw value")]
// SAFETY: `WM` is a plain newtype over the message number, and this one is in the app range.
pub(super) const NOTIFY: co::WM = unsafe { co::WM::from_raw(co::WM::APP.raw() + 2) };
const NANOSECONDS_PER_TICK: u64 = 1_000_000_000 / CLOCK_RATE.unsigned_abs();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    /// Shows the picture at a time, in clock ticks from the first.
    Seek(i64),
    Play,
    Pause,
}

pub(super) enum Event {
    Opened(Timeline),
    /// Playing reached the last picture and stopped.
    Ended,
    Failed(String),
}

/// What the thread hands the view, collected when the view is notified.
struct Shared {
    frame: Mutex<Option<Frame>>,
    events: Mutex<Vec<Event>>,
    posted: AtomicBool,
    /// Behind a lock only because a handle is not `Sync`.
    view: Mutex<w::HWND>,
}

impl Shared {
    fn show(&self, frame: Frame) {
        *self.frame.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
        self.notify();
    }

    fn event(&self, event: Event) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        self.notify();
    }

    /// Posts at most one notice until the view collects it.
    #[expect(unsafe_code, reason = "calls PostMessage")]
    fn notify(&self) {
        if !self.posted.swap(true, Ordering::AcqRel) {
            let view = self.view.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: NOTIFY carries no pointers, and a window that is gone drops the message.
            let _ = unsafe { view.PostMessage(w::msg::Wm::new(NOTIFY, 0, 0)) };
        }
    }
}

/// The thread ends once this is dropped and it finishes the picture in hand.
pub(super) struct Player {
    commands: Sender<Command>,
    shared: Arc<Shared>,
}

impl Player {
    /// Plays the clip at `path`, its pictures scaled to fit within `bounds`, posting to `view`.
    pub(super) fn start(path: PathBuf, bounds: (u32, u32), view: w::HWND) -> Self {
        let (commands, received) = mpsc::channel();
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            posted: AtomicBool::new(false),
            view: Mutex::new(view),
        });
        let theirs = Arc::clone(&shared);
        let started = thread::Builder::new()
            .name("trim player".to_owned())
            .spawn(move || run(&path, bounds, &received, &theirs));
        if let Err(error) = started {
            shared.event(Event::Failed(error.to_string()));
        }
        Self { commands, shared }
    }

    pub(super) fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Takes the latest picture and the events since the last notice, and allows the next.
    pub(super) fn collect(&self) -> (Option<Frame>, Vec<Event>) {
        self.shared.posted.store(false, Ordering::Release);
        let frame = self
            .shared
            .frame
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let events = std::mem::take(
            &mut *self
                .shared
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        (frame, events)
    }
}

fn run(path: &std::path::Path, bounds: (u32, u32), commands: &Receiver<Command>, shared: &Shared) {
    let preview = match Preview::open(path, bounds) {
        Ok(preview) => preview,
        Err(error) => {
            shared.event(Event::Failed(log::chain(&error).to_string()));
            return;
        }
    };
    shared.event(Event::Opened(Timeline::of(&preview.movie().video)));
    let mut playback = Playback {
        preview,
        shared,
        clock: None,
        pending: None,
        position: 0,
        at_end: false,
    };
    playback.handle(Command::Seek(0));
    loop {
        let command = if playback.clock.is_some() {
            match playback.advance(commands) {
                Step::Command(command) => command,
                Step::Shown => continue,
                Step::Closed => return,
            }
        } else {
            match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            }
        };
        let queued: Vec<Command> = std::iter::once(command)
            .chain(commands.try_iter())
            .collect();
        // Only the last of the seeks a drag sends is worth decoding.
        let last_seek = queued
            .iter()
            .rposition(|command| matches!(command, Command::Seek(_)));
        for (position, command) in queued.into_iter().enumerate() {
            if matches!(command, Command::Seek(_)) && Some(position) != last_seek {
                continue;
            }
            playback.handle(command);
        }
    }
}

enum Step {
    Command(Command),
    Shown,
    Closed,
}

/// The picture times are kept to while playing: the time shown when it started.
struct Clock {
    started: Instant,
    from: i64,
}

struct Playback<'a> {
    preview: Preview,
    shared: &'a Shared,
    /// Set while playing.
    clock: Option<Clock>,
    /// A picture decoded while playing that a command came before.
    pending: Option<Frame>,
    position: i64,
    at_end: bool,
}

impl Playback<'_> {
    fn handle(&mut self, command: Command) {
        match command {
            Command::Seek(time) => {
                self.pending = None;
                match self.preview.seek(time) {
                    Ok(Some(frame)) => {
                        if self.clock.is_some() {
                            self.clock = Some(Clock {
                                started: Instant::now(),
                                from: frame.time,
                            });
                        }
                        self.show(frame);
                    }
                    Ok(None) => {}
                    Err(error) => self.fail(&error),
                }
            }
            Command::Play if self.clock.is_none() => {
                if self.at_end {
                    self.handle(Command::Seek(0));
                }
                self.clock = Some(Clock {
                    started: Instant::now(),
                    from: self.position,
                });
            }
            Command::Play => {}
            Command::Pause => self.clock = None,
        }
    }

    /// Decodes the next picture and shows it when it is due, unless a command comes first.
    fn advance(&mut self, commands: &Receiver<Command>) -> Step {
        let next = match self.pending.take() {
            Some(frame) => Ok(Some(frame)),
            None => self.preview.next_frame(),
        };
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.clock = None;
                self.at_end = true;
                self.shared.event(Event::Ended);
                return Step::Shown;
            }
            Err(error) => {
                self.fail(&error);
                return Step::Shown;
            }
        };
        let Some(clock) = &self.clock else {
            self.pending = Some(frame);
            return Step::Shown;
        };
        let ahead = u64::try_from(frame.time - clock.from).unwrap_or(0);
        let due = clock.started + Duration::from_nanos(ahead * NANOSECONDS_PER_TICK);
        match commands.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(command) => {
                self.pending = Some(frame);
                Step::Command(command)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.show(frame);
                Step::Shown
            }
            Err(RecvTimeoutError::Disconnected) => Step::Closed,
        }
    }

    fn show(&mut self, frame: Frame) {
        self.position = frame.time;
        self.at_end = false;
        self.shared.show(frame);
    }

    fn fail(&mut self, error: &crate::gpu::TrimError) {
        self.clock = None;
        self.shared
            .event(Event::Failed(log::chain(error).to_string()));
    }
}
