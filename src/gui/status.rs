//! The Status tab: what is being recorded and how much is buffered.

use super::{
    layout::{
        self, AMBER, BLACK, ExpandableGroup, Frame, GRAY, GREEN, INNER_WIDTH, INNER_X, LINE, RED,
        TintedLabel, VALUE_WIDTH, VALUE_X, label, path_label,
    },
    text,
    theme::{Skin, Surface},
};
use crate::service::Status;
use winsafe::{self as w, gui, prelude::*};

const NOTICE_EXTRA: i32 = 2 * LINE + 4;

#[derive(Clone)]
pub(super) struct StatusPage {
    page: gui::TabPage,
    frames: Vec<Frame>,
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
}

impl StatusPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let recording = RecordingLabels::new(&page);
        let stats = StatsLabels::new(&page);
        let recording_group = expanding_recording_group(&recording, &stats);
        let frames = vec![recording.frame, stats.frame];
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
        }
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    pub(super) fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub(super) fn on_create(&self) {
        layout::fit_titles(&self.frames);
        self.recording_group.on_create();
    }

    pub(super) fn extra(&self) -> i32 {
        self.recording_group.extra()
    }

    /// Reports whether the Recording group changed height.
    pub(super) fn update(&self, status: &Status, hotkey_error: Option<&str>) -> bool {
        self.show_state(status);
        self.show_figures(status);
        self.show_notices(status, hotkey_error)
    }

    fn show_state(&self, status: &Status) {
        let live = status.recording && status.stream.is_some();
        self.state.set_colour(if live { GREEN } else { GRAY });
        self.state.set_text(&text::status(status.state));
    }

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

    fn show_notices(&self, status: &Status, hotkey_error: Option<&str>) -> bool {
        self.notice
            .set_colour(if status.error.is_some() { RED } else { AMBER });
        let notice = notice_text(status, hotkey_error);
        self.recording_group
            .set_message((!notice.is_empty()).then_some(notice.as_str()))
    }
}

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
    fn new(page: &gui::TabPage) -> Self {
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

struct StatsLabels {
    frame: Frame,
    session_written: Row,
    session_saved: Row,
    lifetime_written: Row,
    lifetime_saved: Row,
}

impl StatsLabels {
    fn new(page: &gui::TabPage) -> Self {
        Self {
            frame: Frame::new(page, text::GROUP_STATS, 138, 100),
            session_written: Row::text(page, text::SESSION_WRITTEN, 158),
            session_saved: Row::text(page, text::SESSION_SAVED, 178),
            lifetime_written: Row::text(page, text::LIFETIME_WRITTEN, 198),
            lifetime_saved: Row::text(page, text::LIFETIME_SAVED, 218),
        }
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

fn expanding_recording_group(recording: &RecordingLabels, stats: &StatsLabels) -> ExpandableGroup {
    ExpandableGroup::new(
        recording.frame.clone(),
        recording.notice.label().clone(),
        NOTICE_EXTRA,
    )
    .moving_frame(stats.frame.clone())
    .moving(stats.labels())
}

fn label_colours(tints: [TintedLabel; 2]) -> impl Fn(&w::HWND) -> w::COLORREF + 'static {
    move |hwnd| {
        tints
            .iter()
            .find_map(|tint| tint.colour_of(hwnd))
            .unwrap_or(BLACK)
    }
}

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

fn row_caption(page: &gui::TabPage, caption: &str, y: i32) -> gui::Label {
    label(page, caption, INNER_X, y, VALUE_X - INNER_X, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_line_lists_what_there_is_one_per_line() {
        let mut status = Status::default();
        assert_eq!(notice_text(&status, None), "");
        status.error = Some("capture failed".to_owned());
        assert_eq!(notice_text(&status, None), "capture failed");
        status.notice = Some("sound is off".to_owned());
        assert_eq!(
            notice_text(&status, Some("hotkey taken")),
            "hotkey taken\r\nsound is off\r\ncapture failed"
        );
    }
}
