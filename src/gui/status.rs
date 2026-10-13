//! The Status tab: what is being recorded and how much is buffered.

use super::{
    layout::{
        self, BUTTON_HEIGHT, ExpandableGroup, Frame, INNER_WIDTH, INNER_X, LINE, Painted,
        TintedLabel, VALUE_WIDTH, VALUE_X, button, hidden_label, label, path_label, saturating_i32,
        skin_button,
    },
    text,
    theme::{Skin, Surface, Tone},
};
use crate::service::{State, Status};
use std::rc::Rc;
use winsafe::{gui, prelude::*};

const NOTICE_EXTRA: i32 = 2 * LINE + 4;
const FIRST_ROW: i32 = 20;
const ROW_PITCH: i32 = 20;
const CONTROL_COUNT: i32 = 5;
const CONTROL_GAP: i32 = 6;
const CONTROL_WIDTH: i32 = (INNER_WIDTH - (CONTROL_COUNT - 1) * CONTROL_GAP) / CONTROL_COUNT;

mod rows {
    use super::{BUTTON_HEIGHT, FIRST_ROW, LINE, ROW_PITCH};

    pub(super) const RECORDING_FRAME: i32 = 6;
    pub(super) const STATE: i32 = RECORDING_FRAME + FIRST_ROW;
    pub(super) const SOURCE: i32 = STATE + LINE + 4;
    pub(super) const BACKEND: i32 = SOURCE + ROW_PITCH;
    pub(super) const ENCODER: i32 = BACKEND + ROW_PITCH;
    pub(super) const RATES: i32 = ENCODER + ROW_PITCH;
    pub(super) const NOTICE: i32 = RATES + LINE + 4;
    pub(super) const RECORDING_HEIGHT: i32 = RATES + LINE + 6 - RECORDING_FRAME;
    pub(super) const STATS_FRAME: i32 = RECORDING_FRAME + RECORDING_HEIGHT + 6;
    pub(super) const SESSION_WRITTEN: i32 = STATS_FRAME + FIRST_ROW;
    pub(super) const SESSION_SAVED: i32 = SESSION_WRITTEN + ROW_PITCH;
    pub(super) const LIFETIME_WRITTEN: i32 = SESSION_SAVED + ROW_PITCH;
    pub(super) const LIFETIME_SAVED: i32 = LIFETIME_WRITTEN + ROW_PITCH;
    pub(super) const STATS_HEIGHT: i32 = LIFETIME_SAVED + LINE + 2 - STATS_FRAME;
    pub(super) const BUFFER_FRAME: i32 = STATS_FRAME + STATS_HEIGHT + 6;
    pub(super) const HELD: i32 = BUFFER_FRAME + FIRST_ROW;
    pub(super) const PLACE: i32 = HELD + ROW_PITCH;
    pub(super) const BUFFER_HEIGHT: i32 = PLACE + LINE + 2 - BUFFER_FRAME;
    pub(super) const CONTROLS_FRAME: i32 = BUFFER_FRAME + BUFFER_HEIGHT + 6;
    pub(super) const CONTROLS: i32 = CONTROLS_FRAME + FIRST_ROW - 2;
    pub(super) const CONTROLS_HEIGHT: i32 = CONTROLS + BUTTON_HEIGHT + 8 - CONTROLS_FRAME;
}

/// What the Controls group's buttons ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    Save,
    /// Pause while recording, resume while paused.
    PauseOrResume,
    Restart,
    OpenClip,
    OpenFolder,
}

#[derive(Clone)]
pub(super) struct StatusPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    frames: Vec<Frame>,
    recording_group: ExpandableGroup,
    recording: RecordingLabels,
    stats: StatsLabels,
    buffer: BufferLabels,
    controls: ControlButtons,
}

impl Painted for Inner {
    fn frames(&self) -> &[Frame] {
        &self.frames
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        [&self.recording.state, &self.recording.notice].into_iter()
    }
}

impl StatusPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = layout::page(parent);
        let recording_frame = Frame::new(
            &page,
            text::GROUP_RECORDING,
            rows::RECORDING_FRAME,
            rows::RECORDING_HEIGHT,
        );
        let recording = RecordingLabels::new(&page);
        let stats_frame = Frame::new(
            &page,
            text::GROUP_STATS,
            rows::STATS_FRAME,
            rows::STATS_HEIGHT,
        );
        let stats = StatsLabels::new(&page);
        let buffer_frame = Frame::new(
            &page,
            text::GROUP_BUFFER,
            rows::BUFFER_FRAME,
            rows::BUFFER_HEIGHT,
        );
        let buffer = BufferLabels::new(&page);
        let controls_frame = Frame::new(
            &page,
            text::GROUP_CONTROLS,
            rows::CONTROLS_FRAME,
            rows::CONTROLS_HEIGHT,
        );
        let controls = ControlButtons::new(&page, skin);
        let frames = vec![recording_frame, stats_frame, buffer_frame, controls_frame];
        let recording_group =
            ExpandableGroup::new(0, recording.notice.label().clone(), NOTICE_EXTRA)
                .moving(stats.labels())
                .moving(buffer.labels())
                .moving(controls.buttons());
        let inner = Rc::new(Inner {
            page,
            frames,
            recording_group,
            recording,
            stats,
            buffer,
            controls,
        });
        layout::draw(
            &inner.page,
            inner.page.on(),
            skin,
            Surface::Page,
            Rc::clone(&inner),
        );
        Self(inner)
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.0.page
    }

    pub(super) fn frames(&self) -> &[Frame] {
        &self.0.frames
    }

    pub(super) fn on_create(&self) {
        layout::fit_titles(&self.0.frames);
    }

    pub(super) fn extra(&self) -> i32 {
        self.0.recording_group.extra()
    }

    /// Reports whether the Recording group changed height. The window's own notices come
    /// before the service's.
    pub(super) fn update(&self, status: &Status, window_notices: [Option<&str>; 2]) -> bool {
        self.show_state(status);
        self.show_figures(status);
        self.0.controls.show(status);
        self.show_notices(status, window_notices)
    }

    pub(super) fn on_control(&self, handle: impl Fn(Control) + Clone + 'static) {
        for (button, control) in self.0.controls.each() {
            let handle = handle.clone();
            button.on().bn_clicked(move || {
                handle(control);
                Ok(())
            });
        }
    }

    fn show_state(&self, status: &Status) {
        let live = matches!(status.state, State::Recording(_));
        let state = &self.0.recording.state;
        state.set_tone(if live { Tone::Good } else { Tone::Muted });
        state.set_text(&text::status(&status.state));
    }

    fn show_figures(&self, status: &Status) {
        let recording = &self.0.recording;
        layout::set_text(&recording.source, &text::source(status));
        layout::set_text(&recording.backend, &text::backend(status));
        layout::set_text(&recording.encoder, &text::encoder(status));
        layout::set_text(&recording.rates, &text::rates(status));
        let figures = self.0.stats.figures(status);
        let buffer = self.0.buffer.figures(status);
        for (row, figure) in figures.iter().chain(&buffer) {
            layout::set_text(&row.value, figure);
        }
    }

    fn show_notices(&self, status: &Status, window_notices: [Option<&str>; 2]) -> bool {
        self.0.recording.notice.set_tone(if status.error.is_some() {
            Tone::Error
        } else {
            Tone::Warning
        });
        let notice = notice_text(status, window_notices);
        self.0.recording_group.set_message(
            &self.0.frames,
            (!notice.is_empty()).then_some(notice.as_str()),
        )
    }
}

struct RecordingLabels {
    state: TintedLabel,
    source: gui::Label,
    backend: gui::Label,
    encoder: gui::Label,
    rates: gui::Label,
    notice: TintedLabel,
}

impl RecordingLabels {
    fn new(page: &gui::TabPage) -> Self {
        Self {
            state: TintedLabel::new(
                label(page, "", INNER_X, rows::STATE, INNER_WIDTH, 1),
                Tone::Muted,
            ),
            source: Row::path(page, text::SOURCE, rows::SOURCE).value,
            backend: Row::text(page, text::BACKEND, rows::BACKEND).value,
            encoder: Row::text(page, text::ENCODER, rows::ENCODER).value,
            rates: Row::text(page, text::FRAME_RATE, rows::RATES).value,
            notice: TintedLabel::new(
                hidden_label(page, INNER_X, rows::NOTICE, INNER_WIDTH, 2),
                Tone::Warning,
            ),
        }
    }
}

struct StatsLabels {
    session_written: Row,
    session_saved: Row,
    lifetime_written: Row,
    lifetime_saved: Row,
}

impl StatsLabels {
    fn new(page: &gui::TabPage) -> Self {
        Self {
            session_written: Row::text(page, text::SESSION_WRITTEN, rows::SESSION_WRITTEN),
            session_saved: Row::text(page, text::SESSION_SAVED, rows::SESSION_SAVED),
            lifetime_written: Row::text(page, text::LIFETIME_WRITTEN, rows::LIFETIME_WRITTEN),
            lifetime_saved: Row::text(page, text::LIFETIME_SAVED, rows::LIFETIME_SAVED),
        }
    }

    fn figures(&self, status: &Status) -> [(&Row, String); 4] {
        [
            (&self.session_written, text::session_written(status)),
            (&self.session_saved, text::session_saved(status)),
            (&self.lifetime_written, text::lifetime_written(status)),
            (&self.lifetime_saved, text::lifetime_saved(status)),
        ]
    }

    fn labels(&self) -> impl Iterator<Item = gui::Label> {
        [
            &self.session_written,
            &self.session_saved,
            &self.lifetime_written,
            &self.lifetime_saved,
        ]
        .into_iter()
        .flat_map(Row::labels)
    }
}

struct BufferLabels {
    held: Row,
    place: Row,
}

impl BufferLabels {
    fn new(page: &gui::TabPage) -> Self {
        Self {
            held: Row::text(page, text::BUFFER_HELD, rows::HELD),
            place: Row::text(page, text::BUFFER_PLACE, rows::PLACE),
        }
    }

    fn figures(&self, status: &Status) -> [(&Row, String); 2] {
        [
            (&self.held, text::buffer_held(status)),
            (&self.place, text::buffer_place(status)),
        ]
    }

    fn labels(&self) -> impl Iterator<Item = gui::Label> {
        [&self.held, &self.place].into_iter().flat_map(Row::labels)
    }
}

struct ControlButtons {
    save: gui::Button,
    pause: gui::Button,
    restart: gui::Button,
    open_clip: gui::Button,
    open_folder: gui::Button,
}

impl ControlButtons {
    fn new(page: &gui::TabPage, skin: &Skin) -> Self {
        let mut captions = [
            text::CONTROL_SAVE,
            text::CONTROL_PAUSE,
            text::CONTROL_RESTART,
            text::CONTROL_OPEN_CLIP,
            text::CONTROL_OPEN_FOLDER,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, caption)| {
            let x = INNER_X + saturating_i32(index) * (CONTROL_WIDTH + CONTROL_GAP);
            let button = button(page, caption, x, rows::CONTROLS, CONTROL_WIDTH);
            skin_button(&button, skin);
            button
        });
        let mut next = || captions.next().expect("five captions");
        Self {
            save: next(),
            pause: next(),
            restart: next(),
            open_clip: next(),
            open_folder: next(),
        }
    }

    fn each(&self) -> [(&gui::Button, Control); 5] {
        [
            (&self.save, Control::Save),
            (&self.pause, Control::PauseOrResume),
            (&self.restart, Control::Restart),
            (&self.open_clip, Control::OpenClip),
            (&self.open_folder, Control::OpenFolder),
        ]
    }

    fn buttons(&self) -> impl Iterator<Item = gui::Button> {
        self.each().into_iter().map(|(button, _)| button.clone())
    }

    fn show(&self, status: &Status) {
        let paused = status.state == State::Paused;
        layout::set_text(
            &self.pause,
            if paused {
                text::CONTROL_RESUME
            } else {
                text::CONTROL_PAUSE
            },
        );
        // A stopped worker takes no more commands.
        let alive = status.state != State::Stopped;
        self.pause
            .hwnd()
            .EnableWindow(alive && status.state != State::Unsupported);
        for button in [&self.save, &self.restart] {
            button.hwnd().EnableWindow(alive);
        }
        self.open_clip
            .hwnd()
            .EnableWindow(status.last_clip.is_some());
    }
}

struct Row {
    caption: gui::Label,
    value: gui::Label,
}

impl Row {
    fn text(page: &gui::TabPage, caption: &str, y: i32) -> Self {
        Self {
            caption: row_caption(page, caption, y),
            value: label(page, "", VALUE_X, y, VALUE_WIDTH, 1),
        }
    }

    fn path(page: &gui::TabPage, caption: &str, y: i32) -> Self {
        Self {
            caption: row_caption(page, caption, y),
            value: path_label(page, VALUE_X, y, VALUE_WIDTH),
        }
    }

    fn labels(&self) -> [gui::Label; 2] {
        [self.caption.clone(), self.value.clone()]
    }
}

fn notice_text(status: &Status, window_notices: [Option<&str>; 2]) -> String {
    let notices: Vec<&str> = window_notices
        .into_iter()
        .chain([
            status.nothing_to_save.then_some(text::NOTHING_TO_SAVE),
            status.error.as_deref(),
        ])
        .flatten()
        .collect();
    notices.join("\r\n")
}

fn row_caption(page: &gui::TabPage, caption: &str, y: i32) -> gui::Label {
    label(page, caption, INNER_X, y, VALUE_X - INNER_X, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_sit_where_they_were_laid_by_hand() {
        assert_eq!(
            [
                rows::STATE,
                rows::SOURCE,
                rows::BACKEND,
                rows::ENCODER,
                rows::RATES,
                rows::NOTICE,
            ],
            [26, 48, 68, 88, 108, 130]
        );
        assert_eq!((rows::RECORDING_FRAME, rows::RECORDING_HEIGHT), (6, 126));
        assert_eq!((rows::STATS_FRAME, rows::STATS_HEIGHT), (138, 100));
        assert_eq!(
            (
                rows::BUFFER_FRAME,
                rows::HELD,
                rows::PLACE,
                rows::BUFFER_HEIGHT
            ),
            (244, 264, 284, 60)
        );
        assert_eq!(
            (rows::CONTROLS_FRAME, rows::CONTROLS, rows::CONTROLS_HEIGHT),
            (310, 328, 50)
        );
        assert_eq!(CONTROL_WIDTH, 76);
        assert_eq!(
            [
                rows::SESSION_WRITTEN,
                rows::SESSION_SAVED,
                rows::LIFETIME_WRITTEN,
                rows::LIFETIME_SAVED,
            ],
            [158, 178, 198, 218]
        );
    }

    #[test]
    fn the_notice_line_lists_what_there_is_one_per_line() {
        let mut status = Status::default();
        assert_eq!(notice_text(&status, [None, None]), "");
        status.error = Some("capture failed".to_owned());
        assert_eq!(notice_text(&status, [None, None]), "capture failed");
        status.nothing_to_save = true;
        assert_eq!(
            notice_text(&status, [Some("hotkey taken"), Some("no tray")]),
            format!(
                "hotkey taken\r\nno tray\r\n{}\r\ncapture failed",
                text::NOTHING_TO_SAVE
            )
        );
    }
}
