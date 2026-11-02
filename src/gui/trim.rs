use self::{
    player::{Command, Event, NOTIFY, Player},
    scrubber::{Gesture, Key, Scrubber},
};
use super::{
    layout::{
        self, BUTTON_HEIGHT, Frame as GroupFrame, LINE, MARGIN, Painted, TintedLabel, button,
    },
    text,
    theme::{Skin, Surface, Tone},
};
use crate::{
    gpu::{self, Frame, Job},
    log,
    replay::CLOCK_RATE,
    sys::{callback::Slot, thread as sys_thread},
    trim::{Cut, Timeline},
};
use std::{
    cell::RefCell,
    iter,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};
use winsafe::{self as w, co, gui, prelude::*};

mod player;
mod scrubber;

const SCREEN_WIDTH: i32 = 576;
const SCREEN_HEIGHT: i32 = SCREEN_WIDTH * 9 / 16;
pub(super) const WIDTH: i32 = MARGIN + SCREEN_WIDTH + MARGIN;
const SCRUBBER_Y: i32 = MARGIN + SCREEN_HEIGHT + 8;
const SCRUBBER_HEIGHT: i32 = 32;
const CONTROLS_Y: i32 = SCRUBBER_Y + SCRUBBER_HEIGHT + 8;
const ACTIONS_Y: i32 = CONTROLS_Y + BUTTON_HEIGHT + 8;
pub(super) const HEIGHT: i32 = ACTIONS_Y + BUTTON_HEIGHT + MARGIN;
const PLAY_WIDTH: i32 = 80;
const ACTION_WIDTH: i32 = 96;
const GAP: i32 = 6;
const RIGHT_X: i32 = WIDTH - MARGIN - ACTION_WIDTH;
const LEFT_X: i32 = RIGHT_X - GAP - ACTION_WIDTH;
/// Centres a line of text on a button's row.
const TEXT_DROP: i32 = (BUTTON_HEIGHT - LINE) / 2;
const TIME_X: i32 = MARGIN + PLAY_WIDTH + 2 * GAP;
const EXPORT_TIMER: usize = 1;
const EXPORT_POLL_MS: u32 = 100;

type Action = fn(&TrimView);

/// The clip to trim and where its cuts go.
pub(super) struct Session {
    pub(super) clip: PathBuf,
    pub(super) directory: PathBuf,
    /// The bitrate and frame rate the settings record at.
    pub(super) recorded: (u32, u32),
}

/// The main window's face while a clip is trimmed: the clip playing, its timeline with the cut
/// marked on it, and the buttons that play it, mark it and save it.
#[derive(Clone)]
pub(super) struct TrimView(Rc<Inner>);

/// The state is borrowed briefly, never across a call that can re-enter the window procedure.
struct Inner {
    control: gui::WindowControl,
    screen: gui::WindowControl,
    scrubber: Scrubber,
    play: gui::Button,
    time: gui::Label,
    set_start: gui::Button,
    set_end: gui::Button,
    message: TintedLabel,
    back: gui::Button,
    save: gui::Button,
    state: RefCell<State>,
    leave: Slot<dyn Fn()>,
    saved: Slot<dyn Fn(&Path)>,
}

impl Painted for Inner {
    fn frames(&self) -> &[GroupFrame] {
        &[]
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        iter::once(&self.message)
    }
}

#[derive(Default)]
struct State {
    session: Option<Session>,
    player: Option<Player>,
    timeline: Option<Timeline>,
    /// The time of the picture on the screen.
    position: i64,
    /// The picture last sought, shown on the timeline until it reaches the screen.
    target: Option<i64>,
    cut: Option<Cut>,
    playing: bool,
    frame: Option<Frame>,
    export: Option<Export>,
}

/// A cut being encoded on its own thread.
struct Export {
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    done: Receiver<Finished>,
}

enum Finished {
    Saved(PathBuf),
    Cancelled,
    Failed(String),
}

/// What the controls show, read from the state in one borrow.
struct Shown {
    opened: bool,
    exporting: bool,
    playing: bool,
    duration: i64,
    position: i64,
    cut: Option<Cut>,
}

impl State {
    fn position(&self) -> i64 {
        self.target.unwrap_or(self.position)
    }

    fn shown(&self) -> Shown {
        Shown {
            opened: self.timeline.is_some(),
            exporting: self.export.is_some(),
            playing: self.playing,
            duration: self.timeline.as_ref().map_or(0, Timeline::duration),
            position: self.position(),
            cut: self.cut,
        }
    }

    /// Seeks to the picture shown at `time`.
    fn seek(&mut self, time: i64) -> Option<Command> {
        let shown = self.timeline.as_ref()?.frame_at(time);
        self.target = Some(shown);
        Some(Command::Seek(shown))
    }

    fn step(&mut self, frames: isize) -> Option<Command> {
        let time = self.timeline.as_ref()?.step(self.position(), frames);
        self.seek(time)
    }

    fn jump(&mut self, seconds: isize) -> Option<Command> {
        let seconds = i64::try_from(seconds).ok()?;
        let time = self
            .position()
            .saturating_add(seconds.saturating_mul(CLOCK_RATE));
        self.seek(time.max(0))
    }

    fn last(&mut self) -> Option<Command> {
        let duration = self.timeline.as_ref()?.duration();
        self.seek(duration - 1)
    }

    /// Drags the start to the edge nearest `time`, short of the end, and shows its picture.
    fn drag_start(&mut self, time: i64) -> Option<Command> {
        let (timeline, cut) = (self.timeline.as_ref()?, self.cut?);
        let start = timeline
            .nearest_edge(time)
            .min(timeline.frame_at(cut.end - 1));
        self.cut = Some(Cut { start, ..cut });
        self.seek(start)
    }

    /// Drags the end to the edge nearest `time`, past the start, and shows the last picture kept.
    fn drag_end(&mut self, time: i64) -> Option<Command> {
        let (timeline, cut) = (self.timeline.as_ref()?, self.cut?);
        let end = timeline.nearest_edge(time).max(timeline.end_of(cut.start));
        self.cut = Some(Cut { end, ..cut });
        self.seek(end - 1)
    }

    /// Starts the cut at the picture on screen. A start past the end lets the cut run to the end
    /// of the clip.
    fn mark_start(&mut self) {
        let (Some(timeline), Some(cut)) = (&self.timeline, self.cut) else {
            return;
        };
        let start = timeline.frame_at(self.target.unwrap_or(self.position));
        let end = if start < cut.end {
            cut.end
        } else {
            timeline.duration()
        };
        self.cut = Some(Cut { start, end });
    }

    /// Ends the cut after the picture on screen. An end before the start lets the cut run from
    /// the start of the clip.
    fn mark_end(&mut self) {
        let (Some(timeline), Some(cut)) = (&self.timeline, self.cut) else {
            return;
        };
        let end = timeline.end_of(self.target.unwrap_or(self.position));
        let start = if cut.start < end {
            cut.start
        } else {
            timeline.whole().start
        };
        self.cut = Some(Cut { start, end });
    }

    fn job(&self) -> Option<Job> {
        let session = self.session.as_ref()?;
        Some(Job {
            source: session.clip.clone(),
            cut: self.cut?,
            directory: session.directory.clone(),
            recorded: session.recorded,
        })
    }
}

impl TrimView {
    /// Creates the view hidden, to be shown over the tabs while a clip is trimmed.
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let control = gui::WindowControl::new(
            parent,
            gui::WindowControlOpts {
                position: gui::dpi(0, 0),
                size: gui::dpi(WIDTH, HEIGHT),
                style: co::WS::CHILD | co::WS::CLIPCHILDREN | co::WS::CLIPSIBLINGS,
                ex_style: co::WS_EX::CONTROLPARENT,
                class_bg_brush: gui::Brush::None,
                ..Default::default()
            },
        );
        let screen = gui::WindowControl::new(
            &control,
            gui::WindowControlOpts {
                position: gui::dpi(MARGIN, MARGIN),
                size: gui::dpi(SCREEN_WIDTH, SCREEN_HEIGHT),
                style: co::WS::CHILD | co::WS::VISIBLE | co::WS::CLIPSIBLINGS,
                class_bg_brush: gui::Brush::None,
                ..Default::default()
            },
        );
        let scrubber = Scrubber::new(
            &control,
            (MARGIN, SCRUBBER_Y),
            (SCREEN_WIDTH, SCRUBBER_HEIGHT),
            skin,
        );
        let skinned = |caption: &str, x: i32, y: i32, width: i32| {
            let button = button(&control, caption, x, y, width);
            layout::skin_button(&button, skin);
            button
        };
        let play = skinned(text::TRIM_PLAY, MARGIN, CONTROLS_Y, PLAY_WIDTH);
        let set_start = skinned(text::TRIM_SET_START, LEFT_X, CONTROLS_Y, ACTION_WIDTH);
        let set_end = skinned(text::TRIM_SET_END, RIGHT_X, CONTROLS_Y, ACTION_WIDTH);
        let back = skinned(text::TRIM_BACK, LEFT_X, ACTIONS_Y, ACTION_WIDTH);
        let save = skinned(text::TRIM_SAVE, RIGHT_X, ACTIONS_Y, ACTION_WIDTH);
        let time = layout::label(
            &control,
            "",
            TIME_X,
            CONTROLS_Y + TEXT_DROP,
            LEFT_X - GAP - TIME_X,
            1,
        );
        let message = TintedLabel::new(
            gui::Label::new(
                &control,
                gui::LabelOpts {
                    text: "",
                    position: gui::dpi(MARGIN, ACTIONS_Y + TEXT_DROP),
                    size: gui::dpi(LEFT_X - GAP - MARGIN, LINE),
                    control_style: co::SS::LEFT | co::SS::ENDELLIPSIS,
                    ..Default::default()
                },
            ),
            Tone::Muted,
        );
        let inner = Rc::new(Inner {
            control,
            screen,
            scrubber,
            play,
            time,
            set_start,
            set_end,
            message,
            back,
            save,
            state: RefCell::default(),
            leave: Slot::new(),
            saved: Slot::new(),
        });
        layout::draw(
            &inner.control,
            inner.control.on(),
            skin,
            Surface::Dialog,
            Rc::clone(&inner),
        );
        let view = Self(inner);
        view.events();
        view
    }

    pub(super) fn hwnd(&self) -> &w::HWND {
        self.0.control.hwnd()
    }

    /// Called when Back is pressed outside an export.
    pub(super) fn on_leave(&self, handler: impl Fn() + 'static) {
        self.0.leave.set(Rc::new(handler));
    }

    /// Called with each cut saved.
    pub(super) fn on_saved(&self, handler: impl Fn(&Path) + 'static) {
        self.0.saved.set(Rc::new(handler));
    }

    /// Starts showing the session's clip. The timeline takes the focus once the clip is open.
    #[expect(unsafe_code, reason = "copies the view's handle for the player")]
    pub(super) fn open(&self, session: Session) {
        let bounds = (
            u32::try_from(gui::dpi_x(SCREEN_WIDTH)).unwrap_or(1),
            u32::try_from(gui::dpi_y(SCREEN_HEIGHT)).unwrap_or(1),
        );
        // SAFETY: the copy is only posted to, and a window that is gone drops the message.
        let view = unsafe { self.hwnd().raw_copy() };
        let player = Player::start(session.clip.clone(), bounds, view);
        *self.0.state.borrow_mut() = State {
            session: Some(session),
            player: Some(player),
            ..State::default()
        };
        self.say(Tone::Muted, text::TRIM_OPENING);
        self.show_state();
        let _ = self.0.screen.hwnd().InvalidateRect(None, false);
        let _ = self.0.back.hwnd().SetFocus();
    }

    /// Stops the player and any export, whose cut is then not saved.
    pub(super) fn close(&self) {
        let _ = self.hwnd().KillTimer(EXPORT_TIMER);
        let state = std::mem::take(&mut *self.0.state.borrow_mut());
        if let Some(export) = &state.export {
            export.cancel.store(true, Ordering::Relaxed);
        }
        drop(state);
    }

    pub(super) fn pause(&self) {
        let command = {
            let mut state = self.0.state.borrow_mut();
            std::mem::take(&mut state.playing).then_some(Command::Pause)
        };
        self.send(command);
        self.show_state();
    }

    /// Cancels the export, if one is running, or leaves the view.
    pub(super) fn back(&self) {
        let cancelling = self.0.state.borrow().export.as_ref().map(|export| {
            export.cancel.store(true, Ordering::Relaxed);
        });
        if cancelling.is_none()
            && let Some(leave) = self.0.leave.get()
        {
            leave();
        }
    }

    fn events(&self) {
        let events = self.0.control.on();
        let view = self.clone();
        events.wm(NOTIFY, move |_| {
            view.on_notify();
            Ok(0)
        });
        let view = self.clone();
        events.wm_timer(EXPORT_TIMER, move || {
            view.poll_export();
            Ok(())
        });
        let view = self.clone();
        events.wm_destroy(move || {
            view.close();
            Ok(())
        });

        let screen = self.0.screen.on();
        screen.wm_erase_bkgnd(|_| Ok(1));
        let view = self.clone();
        screen.wm_paint(move || {
            view.paint_screen();
            Ok(())
        });
        let view = self.clone();
        screen.wm_l_button_down(move |_| {
            let _ = view.0.scrubber.hwnd().SetFocus();
            view.play_or_pause();
            Ok(())
        });

        let view = self.clone();
        self.0
            .scrubber
            .on_gesture(move |gesture| view.on_gesture(gesture));
        let view = self.clone();
        self.0.scrubber.on_key(move |key| view.on_key(key));

        let buttons: [(&gui::Button, Action); 5] = [
            (&self.0.play, Self::play_or_pause),
            (&self.0.set_start, |view| view.mark(State::mark_start)),
            (&self.0.set_end, |view| view.mark(State::mark_end)),
            (&self.0.back, Self::back),
            (&self.0.save, Self::save),
        ];
        for (button, action) in buttons {
            let view = self.clone();
            button.on().bn_clicked(move || {
                action(&view);
                Ok(())
            });
        }
    }

    fn on_gesture(&self, gesture: Gesture) {
        let command = {
            let mut state = self.0.state.borrow_mut();
            if state.export.is_some() {
                return;
            }
            match gesture {
                Gesture::Scrub(time) => state.seek(time),
                Gesture::Start(time) => state.drag_start(time),
                Gesture::End(time) => state.drag_end(time),
            }
        };
        self.send(command);
        if !matches!(gesture, Gesture::Scrub(_)) {
            self.say_cut();
        }
        self.show_state();
    }

    fn on_key(&self, key: Key) {
        match key {
            Key::PlayOrPause => self.play_or_pause(),
            Key::MarkStart => self.mark(State::mark_start),
            Key::MarkEnd => self.mark(State::mark_end),
            Key::Leave => self.back(),
            Key::Step(_) | Key::Jump(_) | Key::First | Key::Last => self.move_by(key),
        }
    }

    /// Moves the playhead as `key` asks, pausing first for a single picture.
    fn move_by(&self, key: Key) {
        let commands = {
            let mut state = self.0.state.borrow_mut();
            if state.export.is_some() {
                return;
            }
            let pause = matches!(key, Key::Step(_)) && std::mem::take(&mut state.playing);
            let seek = match key {
                Key::Step(frames) => state.step(frames),
                Key::Jump(seconds) => state.jump(seconds),
                Key::First => state.seek(0),
                _ => state.last(),
            };
            [pause.then_some(Command::Pause), seek]
        };
        for command in commands {
            self.send(command);
        }
        self.show_state();
    }

    fn mark(&self, mark: fn(&mut State)) {
        {
            let mut state = self.0.state.borrow_mut();
            if state.export.is_some() {
                return;
            }
            mark(&mut state);
        }
        self.say_cut();
        self.show_state();
    }

    fn play_or_pause(&self) {
        let command = {
            let mut state = self.0.state.borrow_mut();
            if state.timeline.is_none() || state.export.is_some() {
                return;
            }
            state.playing = !state.playing;
            if state.playing {
                Command::Play
            } else {
                Command::Pause
            }
        };
        self.send(Some(command));
        self.show_state();
    }

    fn send(&self, command: Option<Command>) {
        if let Some(command) = command
            && let Some(player) = &self.0.state.borrow().player
        {
            player.send(command);
        }
    }

    /// Collects what the player has posted.
    fn on_notify(&self) {
        let Some((frame, events)) = self.0.state.borrow().player.as_ref().map(Player::collect)
        else {
            return;
        };
        for event in events {
            match event {
                Event::Opened(timeline) => {
                    {
                        let mut state = self.0.state.borrow_mut();
                        state.cut = Some(timeline.whole());
                        state.timeline = Some(timeline);
                    }
                    self.say_cut();
                    self.show_state();
                    let _ = self.0.scrubber.hwnd().SetFocus();
                }
                Event::Ended => self.0.state.borrow_mut().playing = false,
                Event::Failed(reason) => self.player_failed(&reason),
            }
        }
        if let Some(frame) = frame {
            let mut state = self.0.state.borrow_mut();
            if state.target == Some(frame.time) {
                state.target = None;
            }
            state.position = frame.time;
            state.frame = Some(frame);
        }
        let _ = self.0.screen.hwnd().InvalidateRect(None, false);
        self.show_state();
    }

    fn player_failed(&self, reason: &str) {
        let clip = {
            let mut state = self.0.state.borrow_mut();
            state.playing = false;
            state.session.as_ref().map(|session| session.clip.clone())
        };
        if let Some(clip) = clip {
            log::warning(text::trim_unplayable_log(&clip, reason));
        }
        self.say(Tone::Error, &text::trim_unplayable(reason));
    }

    fn save(&self) {
        let job = {
            let mut state = self.0.state.borrow_mut();
            if state.export.is_some() {
                return;
            }
            let Some(job) = state.job() else {
                return;
            };
            if std::mem::take(&mut state.playing)
                && let Some(player) = &state.player
            {
                player.send(Command::Pause);
            }
            job
        };
        let source = job.source.clone();
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (finished, done) = mpsc::channel();
        let (theirs, stop) = (Arc::clone(&progress), Arc::clone(&cancel));
        let started = thread::Builder::new()
            .name("trim export".to_owned())
            .spawn(move || {
                let _ = sys_thread::prefer_efficiency();
                let result = match gpu::export(&job, &theirs, &stop) {
                    Ok(path) => Finished::Saved(path),
                    Err(error) if error.is_cancelled() => Finished::Cancelled,
                    Err(error) => Finished::Failed(log::chain(&error).to_string()),
                };
                let _ = finished.send(result);
            });
        if let Err(error) = started {
            self.finish(&source, Finished::Failed(error.to_string()));
            return;
        }
        self.0.state.borrow_mut().export = Some(Export {
            progress,
            cancel,
            done,
        });
        let _ = self.hwnd().SetTimer(EXPORT_TIMER, EXPORT_POLL_MS, None);
        self.say(Tone::Muted, &text::trim_progress(0));
        self.show_state();
        let _ = self.0.back.hwnd().SetFocus();
    }

    fn poll_export(&self) {
        let (progress, done, source) = {
            let state = self.0.state.borrow();
            let Some(export) = &state.export else {
                drop(state);
                let _ = self.hwnd().KillTimer(EXPORT_TIMER);
                return;
            };
            (
                export.progress.load(Ordering::Relaxed),
                export.done.try_recv(),
                state.session.as_ref().map(|session| session.clip.clone()),
            )
        };
        let finished = match done {
            Err(TryRecvError::Empty) => {
                self.say(
                    Tone::Muted,
                    &text::trim_progress(progress * 100 / gpu::PROGRESS_SCALE),
                );
                return;
            }
            Ok(finished) => finished,
            Err(TryRecvError::Disconnected) => Finished::Failed(text::TRIM_STOPPED.to_owned()),
        };
        let _ = self.hwnd().KillTimer(EXPORT_TIMER);
        self.0.state.borrow_mut().export = None;
        self.finish(&source.unwrap_or_default(), finished);
        self.show_state();
        let _ = self.0.scrubber.hwnd().SetFocus();
    }

    fn finish(&self, source: &Path, finished: Finished) {
        match finished {
            Finished::Saved(path) => {
                log::info(text::trim_saved_log(source, &path));
                self.say(Tone::Good, &text::trim_saved(&path));
                if let Some(saved) = self.0.saved.get() {
                    saved(&path);
                }
            }
            Finished::Cancelled => self.say(Tone::Muted, text::TRIM_CANCELLED),
            Finished::Failed(reason) => {
                log::warning(text::trim_failed_log(source, &reason));
                self.say(Tone::Error, &text::trim_failed(&reason));
            }
        }
    }

    fn say(&self, tone: Tone, message: &str) {
        self.0.message.set_tone(tone);
        self.0.message.set_text(message);
    }

    fn say_cut(&self) {
        let cut = self.0.state.borrow().cut;
        if let Some(cut) = cut {
            self.say(Tone::Muted, &text::trim_cut(cut));
        }
    }

    fn show_state(&self) {
        let shown = self.0.state.borrow().shown();
        self.0
            .scrubber
            .show(shown.duration, shown.position, shown.cut);
        let time = if shown.opened {
            text::trim_position(shown.position, shown.duration)
        } else {
            String::new()
        };
        layout::set_text(&self.0.time, &time);
        layout::set_text(
            &self.0.play,
            if shown.playing {
                text::TRIM_PAUSE
            } else {
                text::TRIM_PLAY
            },
        );
        layout::set_text(
            &self.0.back,
            if shown.exporting {
                text::TRIM_CANCEL
            } else {
                text::TRIM_BACK
            },
        );
        let usable = shown.opened && !shown.exporting;
        for window in [
            self.0.scrubber.hwnd(),
            self.0.play.hwnd(),
            self.0.set_start.hwnd(),
            self.0.set_end.hwnd(),
            self.0.save.hwnd(),
        ] {
            if window.IsWindowEnabled() != usable {
                window.EnableWindow(usable);
            }
        }
    }

    #[expect(unsafe_code, reason = "calls StretchDIBits")]
    fn paint_screen(&self) {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, SRCCOPY, StretchDIBits,
        };

        let hwnd = self.0.screen.hwnd();
        let Ok(paint) = hwnd.BeginPaint() else {
            return;
        };
        let Ok(client) = hwnd.GetClientRect() else {
            return;
        };
        let Ok(black) = w::HBRUSH::GetStockObject(co::STOCK_BRUSH::BLACK) else {
            return;
        };
        let state = self.0.state.borrow();
        let picture = state.frame.as_ref().and_then(|frame| {
            let size = (
                i32::try_from(frame.width).ok()?,
                i32::try_from(frame.height).ok()?,
            );
            Some((frame, size))
        });
        let Some((frame, (width, height))) = picture else {
            let _ = paint.FillRect(client, &black);
            return;
        };
        let shown = letterbox((client.right, client.bottom), (width, height));
        for bar in bars(client, shown) {
            let _ = paint.FillRect(bar, &black);
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
                biWidth: width,
                // A negative height makes the rows run top to bottom.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let _ = paint.SetStretchBltMode(co::STRETCH_MODE::COLORONCOLOR);
        // SAFETY: the DC is the live paint target, and `info` describes `frame.bgra`, which is
        // top-down 32-bit pixels of the size given and outlives the call.
        unsafe {
            StretchDIBits(
                HDC(paint.ptr()),
                shown.left,
                shown.top,
                shown.right - shown.left,
                shown.bottom - shown.top,
                0,
                0,
                width,
                height,
                Some(frame.bgra.as_ptr().cast()),
                &raw const info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
}

/// The largest rectangle of the picture's shape centred in the client.
fn letterbox((width, height): (i32, i32), (picture_width, picture_height): (i32, i32)) -> w::RECT {
    let (width, height) = (i64::from(width), i64::from(height));
    let (picture_width, picture_height) = (
        i64::from(picture_width.max(1)),
        i64::from(picture_height.max(1)),
    );
    let (shown_width, shown_height) = if width * picture_height <= height * picture_width {
        (width, picture_height * width / picture_width)
    } else {
        (picture_width * height / picture_height, height)
    };
    let left = (width - shown_width) / 2;
    let top = (height - shown_height) / 2;
    let edge = |value: i64| i32::try_from(value).unwrap_or(0);
    w::RECT {
        left: edge(left),
        top: edge(top),
        right: edge(left + shown_width),
        bottom: edge(top + shown_height),
    }
}

/// The parts of the client around the picture.
fn bars(client: w::RECT, shown: w::RECT) -> impl Iterator<Item = w::RECT> {
    [
        w::RECT {
            bottom: shown.top,
            ..client
        },
        w::RECT {
            top: shown.bottom,
            ..client
        },
        w::RECT {
            top: shown.top,
            right: shown.left,
            bottom: shown.bottom,
            ..client
        },
        w::RECT {
            left: shown.right,
            top: shown.top,
            bottom: shown.bottom,
            ..client
        },
    ]
    .into_iter()
    .filter(|bar| bar.right > bar.left && bar.bottom > bar.top)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corners(rect: w::RECT) -> (i32, i32, i32, i32) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    #[test]
    fn the_view_fits_a_sixteen_by_nine_screen_and_two_rows_of_buttons() {
        assert_eq!((SCREEN_WIDTH, SCREEN_HEIGHT), (576, 324));
        assert_eq!((WIDTH, HEIGHT), (592, 444));
        const { assert!(TIME_X + 120 <= LEFT_X - GAP) };
    }

    #[test]
    fn a_picture_is_letterboxed_or_pillarboxed_to_keep_its_shape() {
        assert_eq!(
            corners(letterbox((576, 324), (1280, 720))),
            (0, 0, 576, 324)
        );
        assert_eq!(
            corners(letterbox((576, 324), (1280, 1024))),
            (85, 0, 490, 324)
        );
        assert_eq!(
            corners(letterbox((576, 324), (2560, 1080))),
            (0, 40, 576, 283)
        );
        assert_eq!(corners(letterbox((576, 324), (320, 180))), (0, 0, 576, 324));
    }

    #[test]
    fn the_bars_cover_the_client_around_the_picture_and_nothing_else() {
        let client = w::RECT {
            left: 0,
            top: 0,
            right: 576,
            bottom: 324,
        };
        let pillarboxed: Vec<_> = bars(client, letterbox((576, 324), (1280, 1024)))
            .map(corners)
            .collect();
        assert_eq!(pillarboxed, [(0, 0, 85, 324), (490, 0, 576, 324)]);
        assert_eq!(bars(client, client).count(), 0);
    }
}
