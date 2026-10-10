//! The Status tab: what is being recorded, how much of it is buffered, and the last saved clip,
//! with buttons to save a replay and to stop or start recording.

use super::{
    layout::{
        self, AMBER, BLACK, ExpandableGroup, Frame, GRAY, GREEN, INNER_WIDTH, INNER_X, LINE, RED,
        TintedLabel, VALUE_WIDTH, VALUE_X, button, label, path_label, skin_button,
    },
    text,
    theme::{Skin, Surface},
};
use crate::{
    config::Config,
    service::{DisplayStatus, Status},
};
use winsafe::{gui, prelude::*};

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
    session_written: gui::Label,
    session_saved: gui::Label,
    lifetime_written: gui::Label,
    lifetime_saved: gui::Label,
    notice: TintedLabel,
    last_clip: gui::Label,
    folder: gui::Label,
    pub(super) save: gui::Button,
    pub(super) recording: gui::Button,
}

impl StatusPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let caption = |text, y| label(&page, text, INNER_X, y, VALUE_X - INNER_X, 1);

        // Compact; the group adds two lines for the notice label when needed.
        let recording_frame = Frame::new(&page, text::GROUP_RECORDING, 6, 126);
        let state = TintedLabel::new(&page, INNER_X, 26, INNER_WIDTH, 1, GRAY);
        let _source_caption = caption(text::SOURCE, 48);
        let source = path_label(&page, VALUE_X, 48, VALUE_WIDTH);
        let _capture_caption = caption(text::BACKEND, 68);
        let backend = label(&page, "", VALUE_X, 68, VALUE_WIDTH, 1);
        let _encoder_caption = caption(text::ENCODER, 88);
        let encoder = label(&page, "", VALUE_X, 88, VALUE_WIDTH, 1);
        let _rates_caption = caption(text::CAPTURE_RATE, 108);
        let rates = label(&page, "", VALUE_X, 108, VALUE_WIDTH, 1);
        let notice = TintedLabel::new(&page, INNER_X, 130, INNER_WIDTH, 2, AMBER);

        // This run's figures, then the lifetime ones kept across runs.
        let stats_frame = Frame::new(&page, text::GROUP_STATS, 138, 100);
        let session_written_caption = caption(text::SESSION_WRITTEN, 158);
        let session_written = label(&page, "", VALUE_X, 158, VALUE_WIDTH, 1);
        let session_saved_caption = caption(text::SESSION_SAVED, 178);
        let session_saved = label(&page, "", VALUE_X, 178, VALUE_WIDTH, 1);
        let lifetime_written_caption = caption(text::LIFETIME_WRITTEN, 198);
        let lifetime_written = label(&page, "", VALUE_X, 198, VALUE_WIDTH, 1);
        let lifetime_saved_caption = caption(text::LIFETIME_SAVED, 218);
        let lifetime_saved = label(&page, "", VALUE_X, 218, VALUE_WIDTH, 1);

        let clips_frame = Frame::new(&page, text::GROUP_CLIPS, 244, 100);
        let last_clip_caption = caption(text::LAST_CLIP, 264);
        let last_clip = path_label(&page, VALUE_X, 264, VALUE_WIDTH);
        let folder_caption = caption(text::CLIP_FOLDER, 284);
        let folder = path_label(&page, VALUE_X, 284, VALUE_WIDTH);
        let save = button(&page, text::SAVE_REPLAY, INNER_X, 310, 120);
        let recording = button(
            &page,
            text::STOP_RECORDING,
            INNER_X + INNER_WIDTH - 120,
            310,
            120,
        );

        skin_button(&save, skin, true);
        skin_button(&recording, skin, false);

        let recording_group = ExpandableGroup::new(
            recording_frame.clone(),
            notice.label().clone(),
            NOTICE_EXTRA,
        )
        .moving_frame(stats_frame.clone())
        .moving([
            session_written_caption,
            session_written.clone(),
            session_saved_caption,
            session_saved.clone(),
            lifetime_written_caption,
            lifetime_written.clone(),
            lifetime_saved_caption,
            lifetime_saved.clone(),
        ])
        .moving_frame(clips_frame.clone())
        .moving([
            last_clip_caption,
            last_clip.clone(),
            folder_caption,
            folder.clone(),
        ])
        .moving([save.clone(), recording.clone()]);
        let frames = vec![recording_frame, stats_frame, clips_frame];

        // The tinted lines read their colour here; every other label is black.
        let tints = [state.clone(), notice.clone()];
        layout::draw(
            &page,
            page.on(),
            frames.clone(),
            skin,
            Surface::Page,
            move |hwnd| {
                tints
                    .iter()
                    .find_map(|tint| tint.colour_of(hwnd))
                    .unwrap_or(BLACK)
            },
        );

        Self {
            page,
            frames,
            recording_group,
            state,
            source,
            backend,
            encoder,
            rates,
            session_written,
            session_saved,
            lifetime_written,
            lifetime_saved,
            notice,
            last_clip,
            folder,
            save,
            recording,
        }
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

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
        let live = status.recording && status.stream.is_some();
        self.state.set_colour(if live { GREEN } else { GRAY });
        self.notice
            .set_colour(if status.error.is_some() { RED } else { AMBER });

        self.state.set_text(&text::status(status.state));
        layout::set_text(&self.source, &text::source(status));
        layout::set_text(&self.backend, &text::capture(status));
        layout::set_text(&self.encoder, &text::encoder(status));
        layout::set_text(&self.rates, &text::rates(status));
        layout::set_text(&self.session_written, &text::session_written(status));
        layout::set_text(&self.session_saved, &text::session_saved(status));
        layout::set_text(&self.lifetime_written, &text::lifetime_written(status));
        layout::set_text(&self.lifetime_saved, &text::saved(status));

        let notices: Vec<&str> = [
            hotkey_error,
            status.notice.as_deref(),
            status.error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let notice = notices.join("\r\n");
        let grown = self
            .recording_group
            .set_message((!notice.is_empty()).then_some(notice.as_str()));

        layout::set_text(
            &self.last_clip,
            &text::last_clip(status.last_clip.as_deref()),
        );
        layout::set_text(&self.folder, &config.clip_directory().display().to_string());
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
            if status.recording || status.state == DisplayStatus::Waiting {
                text::STOP_RECORDING
            } else {
                text::START_RECORDING
            },
        );
        grown
    }
}
