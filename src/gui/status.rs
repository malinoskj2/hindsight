//! The Status tab: what is being recorded, how much of it is buffered, and the last saved clip,
//! with buttons to save a replay and to stop or start recording.

use super::{
    layout::{
        self, AMBER, BLACK, BUTTON_WIDTH, ExpandableGroup, Frame, GRAY, GREEN, INNER_WIDTH,
        INNER_X, LINE, RED, TintedLabel, VALUE_WIDTH, VALUE_X, button, label, path_label,
        skin_button,
    },
    text,
    theme::{ButtonRole, Skin, Surface},
};
use crate::{
    config::Config,
    service::{DisplayStatus, Status},
};
use winsafe::{self as w, gui, prelude::*};

/// Height the Recording group gains while it shows notices or errors (two lines).
const NOTICE_EXTRA: i32 = 2 * LINE + 4;

#[derive(Clone)]
pub(super) struct StatusPage {
    page: gui::TabPage,
    frames: Vec<Frame>,
    /// The Recording group; it grows while the notice label has something to show.
    recording_group: ExpandableGroup,
    state: TintedLabel,
    source: gui::Label,
    backend: gui::Label,
    encoder: gui::Label,
    rates: gui::Label,
    notice: TintedLabel,
    session_written: gui::Label,
    session_saved: gui::Label,
    lifetime_written: gui::Label,
    lifetime_saved: gui::Label,
    last_clip: gui::Label,
    folder: gui::Label,
    pub(super) save: gui::Button,
    pub(super) recording: gui::Button,
}

impl StatusPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let recording = RecordingLabels::new(&page);
        let stats = StatsLabels::new(&page);
        let clips = ClipsControls::new(&page, skin);
        let recording_group = expanding_recording_group(&recording, &stats, &clips);
        let frames = vec![recording.frame, stats.frame, clips.frame];
        layout::draw(
            &page,
            page.on(),
            frames.clone(),
            skin,
            Surface::Page,
            label_colours([recording.state.clone(), recording.notice.clone()]),
        );

        Self {
            page,
            frames,
            recording_group,
            state: recording.state,
            source: recording.source,
            backend: recording.backend,
            encoder: recording.encoder,
            rates: recording.rates,
            notice: recording.notice,
            session_written: stats.session_written.value,
            session_saved: stats.session_saved.value,
            lifetime_written: stats.lifetime_written.value,
            lifetime_saved: stats.lifetime_saved.value,
            last_clip: clips.last_clip.value,
            folder: clips.folder.value,
            save: clips.save,
            recording: clips.recording,
        }
    }

    /// The tab page the controls sit on.
    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    /// The page's groups, top to bottom.
    pub(super) fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Finishes the parts that need the controls to exist.
    pub(super) fn on_create(&self) {
        layout::fit_titles(&self.frames);
        self.recording_group.on_create();
    }

    /// The extra height the page currently needs for its expanded group, in logical pixels.
    pub(super) fn extra(&self) -> i32 {
        self.recording_group.extra()
    }

    /// Shows `status`, with the clip settings of `config` and `hotkey_error` if the hotkey
    /// could not be registered, and reports whether the Recording group changed height.
    pub(super) fn update(
        &self,
        status: &Status,
        config: &Config,
        hotkey_error: Option<&str>,
    ) -> bool {
        self.show_state(status);
        self.show_figures(status);
        let grown = self.show_notices(status, hotkey_error);
        self.show_clips(status, config);
        self.show_buttons(status);
        grown
    }

    /// The state line, green while frames are being encoded and gray otherwise.
    fn show_state(&self, status: &Status) {
        let live = status.recording && status.stream.is_some();
        self.state.set_colour(if live { GREEN } else { GRAY });
        self.state.set_text(&text::status(status.state));
    }

    /// What is captured and how, and the session and lifetime totals.
    fn show_figures(&self, status: &Status) {
        layout::set_text(&self.source, &text::source(status));
        layout::set_text(&self.backend, &text::backend(status));
        layout::set_text(&self.encoder, &text::encoder(status));
        layout::set_text(&self.rates, &text::rates(status));
        layout::set_text(&self.session_written, &text::session_written(status));
        layout::set_text(&self.session_saved, &text::session_saved(status));
        layout::set_text(&self.lifetime_written, &text::lifetime_written(status));
        layout::set_text(&self.lifetime_saved, &text::lifetime_saved(status));
    }

    /// The notice line, red while it carries an error and amber otherwise; reports whether the
    /// Recording group changed height to make room for it or to give the room back.
    fn show_notices(&self, status: &Status, hotkey_error: Option<&str>) -> bool {
        self.notice
            .set_colour(if status.error.is_some() { RED } else { AMBER });
        let notice = notice_text(status, hotkey_error);
        self.recording_group
            .set_message((!notice.is_empty()).then_some(notice.as_str()))
    }

    /// The last saved clip and the folder clips go to.
    fn show_clips(&self, status: &Status, config: &Config) {
        layout::set_text(
            &self.last_clip,
            &text::last_clip(status.last_clip.as_deref()),
        );
        layout::set_text(&self.folder, &config.clip_directory().display().to_string());
    }

    /// The save button, disabled while a clip is being saved, and the recording switch.
    fn show_buttons(&self, status: &Status) {
        layout::set_text(
            &self.save,
            if status.saving {
                text::SAVING
            } else {
                text::SAVE_REPLAY
            },
        );
        self.save.hwnd().EnableWindow(!status.saving);
        layout::set_text(
            &self.recording,
            if is_on(status) {
                text::STOP_RECORDING
            } else {
                text::START_RECORDING
            },
        );
    }
}

/// The Recording group: the state line, what is captured and how, and the notice line.
struct RecordingLabels {
    frame: Frame,
    state: TintedLabel,
    source: gui::Label,
    backend: gui::Label,
    encoder: gui::Label,
    rates: gui::Label,
    notice: TintedLabel,
}

impl RecordingLabels {
    /// Lays out the Recording group on `page`.
    fn new(page: &gui::TabPage) -> Self {
        // Compact; the group adds two lines for the notice label when needed.
        let frame = Frame::new(page, text::GROUP_RECORDING, 6, 126);
        let state = TintedLabel::new(page, INNER_X, 26, INNER_WIDTH, 1, GRAY);
        let source = Row::path(page, text::SOURCE, 48).value;
        let backend = Row::text(page, text::BACKEND, 68).value;
        let encoder = Row::text(page, text::ENCODER, 88).value;
        let rates = Row::text(page, text::CAPTURE_RATE, 108).value;
        let notice = TintedLabel::new(page, INNER_X, 130, INNER_WIDTH, 2, AMBER);
        Self {
            frame,
            state,
            source,
            backend,
            encoder,
            rates,
            notice,
        }
    }
}

/// The Stats group: this run's figures, then the lifetime ones kept across runs.
struct StatsLabels {
    frame: Frame,
    session_written: Row,
    session_saved: Row,
    lifetime_written: Row,
    lifetime_saved: Row,
}

impl StatsLabels {
    /// Lays out the Stats group on `page`.
    fn new(page: &gui::TabPage) -> Self {
        Self {
            frame: Frame::new(page, text::GROUP_STATS, 138, 100),
            session_written: Row::text(page, text::SESSION_WRITTEN, 158),
            session_saved: Row::text(page, text::SESSION_SAVED, 178),
            lifetime_written: Row::text(page, text::LIFETIME_WRITTEN, 198),
            lifetime_saved: Row::text(page, text::LIFETIME_SAVED, 218),
        }
    }

    /// Every caption and value, top to bottom.
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

/// The Clips group: the last clip, the clip folder, and the buttons to save a replay and to
/// stop or start recording.
struct ClipsControls {
    frame: Frame,
    last_clip: Row,
    folder: Row,
    save: gui::Button,
    recording: gui::Button,
}

impl ClipsControls {
    /// Lays out the Clips group on `page`, its buttons in the look of `skin`.
    fn new(page: &gui::TabPage, skin: &Skin) -> Self {
        let frame = Frame::new(page, text::GROUP_CLIPS, 244, 100);
        let last_clip = Row::path(page, text::LAST_CLIP, 264);
        let folder = Row::path(page, text::CLIP_FOLDER, 284);
        let save = button(page, text::SAVE_REPLAY, INNER_X, 310, BUTTON_WIDTH);
        let recording = button(
            page,
            text::STOP_RECORDING,
            INNER_X + INNER_WIDTH - BUTTON_WIDTH,
            310,
            BUTTON_WIDTH,
        );
        skin_button(&save, skin, ButtonRole::Primary);
        skin_button(&recording, skin, ButtonRole::Secondary);
        Self {
            frame,
            last_clip,
            folder,
            save,
            recording,
        }
    }
}

/// A caption with the value it names to its right.
struct Row {
    caption: gui::Label,
    value: gui::Label,
}

impl Row {
    /// A row at `y` whose value is plain text.
    fn text(page: &gui::TabPage, caption: &str, y: i32) -> Self {
        Self {
            caption: row_caption(page, caption, y),
            value: label(page, "", VALUE_X, y, VALUE_WIDTH, 1),
        }
    }

    /// A row at `y` whose value is a path, shortened in the middle when it is too long.
    fn path(page: &gui::TabPage, caption: &str, y: i32) -> Self {
        Self {
            caption: row_caption(page, caption, y),
            value: path_label(page, VALUE_X, y, VALUE_WIDTH),
        }
    }

    /// The caption and the value, in that order.
    fn labels(&self) -> [gui::Label; 2] {
        [self.caption.clone(), self.value.clone()]
    }
}

/// Whether recording is switched on: capturing, or waiting for a full-screen app to capture.
pub(super) fn is_on(status: &Status) -> bool {
    status.recording || status.state == DisplayStatus::Waiting
}

/// The Recording group, which grows to show its notice line and moves the Stats and Clips
/// groups down to make room.
fn expanding_recording_group(
    recording: &RecordingLabels,
    stats: &StatsLabels,
    clips: &ClipsControls,
) -> ExpandableGroup {
    ExpandableGroup::new(
        recording.frame.clone(),
        recording.notice.label().clone(),
        NOTICE_EXTRA,
    )
    .moving_frame(stats.frame.clone())
    .moving(stats.labels())
    .moving_frame(clips.frame.clone())
    .moving(
        [&clips.last_clip, &clips.folder]
            .into_iter()
            .flat_map(Row::labels),
    )
    .moving([clips.save.clone(), clips.recording.clone()])
}

/// The colour of each label on the page: the `tints` lines read theirs from beside them; every
/// other label is black.
fn label_colours(tints: [TintedLabel; 2]) -> impl Fn(&w::HWND) -> w::COLORREF + 'static {
    move |hwnd| {
        tints
            .iter()
            .find_map(|tint| tint.colour_of(hwnd))
            .unwrap_or(BLACK)
    }
}

/// The notice line's text: why the hotkey is not registered, the worker's notice and its
/// error, whichever there are, one per line.
fn notice_text(status: &Status, hotkey_error: Option<&str>) -> String {
    let notices: Vec<&str> = [
        hotkey_error,
        status.notice.as_deref(),
        status.error.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    notices.join("\r\n")
}

/// A row's caption, in the column left of its value.
fn row_caption(page: &gui::TabPage, caption: &str, y: i32) -> gui::Label {
    label(page, caption, INNER_X, y, VALUE_X - INNER_X, 1)
}
