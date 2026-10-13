//! The Status tab: what is being recorded and how much is buffered.

use super::{
    layout::{
        self, AMBER, ExpandableGroup, Frame, GRAY, GREEN, INNER_WIDTH, INNER_X, LINE, Painted, RED,
        TintedLabel, VALUE_WIDTH, VALUE_X, hidden_label, label, path_label,
    },
    text,
    theme::{Skin, Surface},
};
use crate::service::Status;
use std::rc::Rc;
use winsafe::{gui, prelude::*};

const NOTICE_EXTRA: i32 = 2 * LINE + 4;
const FIRST_ROW: i32 = 20;
const ROW_PITCH: i32 = 20;

mod rows {
    use super::{FIRST_ROW, LINE, ROW_PITCH};

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
}

#[derive(Clone)]
pub(super) struct StatusPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    frames: Vec<Frame>,
    recording_group: ExpandableGroup,
    recording: RecordingLabels,
    stats: StatsLabels,
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
        let frames = vec![recording_frame, stats_frame];
        let recording_group =
            ExpandableGroup::new(0, recording.notice.label().clone(), NOTICE_EXTRA)
                .moving(stats.labels());
        let inner = Rc::new(Inner {
            page,
            frames,
            recording_group,
            recording,
            stats,
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
        self.show_notices(status, window_notices)
    }

    fn show_state(&self, status: &Status) {
        let live = status.recording && status.stream.is_some();
        let state = &self.0.recording.state;
        state.set_colour(if live { GREEN } else { GRAY });
        state.set_text(&text::status(status.state));
    }

    fn show_figures(&self, status: &Status) {
        let recording = &self.0.recording;
        layout::set_text(&recording.source, &text::source(status));
        layout::set_text(&recording.backend, &text::backend(status));
        layout::set_text(&recording.encoder, &text::encoder(status));
        layout::set_text(&recording.rates, &text::rates(status));
        for (row, figure) in self.0.stats.figures(status) {
            layout::set_text(&row.value, &figure);
        }
    }

    fn show_notices(&self, status: &Status, window_notices: [Option<&str>; 2]) -> bool {
        self.0
            .recording
            .notice
            .set_colour(if status.error.is_some() { RED } else { AMBER });
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
            state: TintedLabel::new(label(page, "", INNER_X, rows::STATE, INNER_WIDTH, 1), GRAY),
            source: Row::path(page, text::SOURCE, rows::SOURCE).value,
            backend: Row::text(page, text::BACKEND, rows::BACKEND).value,
            encoder: Row::text(page, text::ENCODER, rows::ENCODER).value,
            rates: Row::text(page, text::FRAME_RATE, rows::RATES).value,
            notice: TintedLabel::new(
                hidden_label(page, INNER_X, rows::NOTICE, INNER_WIDTH, 2),
                AMBER,
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
        .chain([status.notice.as_deref(), status.error.as_deref()])
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
        status.notice = Some("sound is off".to_owned());
        assert_eq!(
            notice_text(&status, [Some("hotkey taken"), Some("no tray")]),
            "hotkey taken\r\nno tray\r\nsound is off\r\ncapture failed"
        );
    }
}
