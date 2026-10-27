//! The Settings tab.

use super::{
    layout::{
        self, BUTTON_HEIGHT, BUTTON_WIDTH, ExpandableGroup, Frame, INNER_WIDTH, INNER_X, LINE,
        Painted, TintedLabel, button, hidden_label, label, saturating_i32, skin_button,
    },
    pane::{self, FIRST_FRAME, FIRST_ROW, Pane},
    text,
    theme::{Skin, Tone},
};
use crate::{
    config::{
        self, Backend, Codec, Config, ConfigError, MAX_BITRATE_MBPS, MAX_BUFFER_SECONDS,
        MAX_FRAME_RATE, MIN_BITRATE_MBPS, MIN_BUFFER_SECONDS, MIN_FRAME_RATE, Memory, Mode, Theme,
    },
    log,
    sys::{
        hotkey::{self, RegisterError},
        shell, startup,
    },
};
use std::{path::PathBuf, rc::Rc};
use thiserror::Error;
use winsafe::{co, gui, prelude::*};

const STARTUP_ERROR_EXTRA: i32 = LINE + 4;
const COLUMN_X: i32 = INNER_X + 204;
const FIELD_OFFSET: i32 = 96;
const NUMBER_WIDTH: i32 = 52;
const CODEC_WIDTH: i32 = 90;
const HOTKEY_WIDTH: i32 = 100;
const CHOICE_WIDTH: i32 = 190;
/// The width of a field that reaches the group's right edge from the first column.
const FULL_WIDTH: i32 = INNER_WIDTH - FIELD_OFFSET;
/// The same width from the second column.
const COLUMN_WIDTH: i32 = INNER_WIDTH - (COLUMN_X - INNER_X) - FIELD_OFFSET;
/// The width of a check box in the second column, which has no caption beside it.
const CHECK_COLUMN_WIDTH: i32 = INNER_WIDTH - (COLUMN_X - INNER_X);
const ERROR_X: i32 = BUTTON_WIDTH + 8;
const BROWSE_WIDTH: i32 = 76;
const BROWSE_GAP: i32 = 6;
/// The width of the clip folder field, which leaves room for the Browse button at the group's
/// right edge.
const FOLDER_WIDTH: i32 = FULL_WIDTH - BROWSE_WIDTH - BROWSE_GAP;
const BROWSE_X: i32 = INNER_X + INNER_WIDTH - BROWSE_WIDTH;
const FIELD_PITCH: i32 = 28;
const CHECK_PITCH: i32 = LINE + 4;

mod rows {
    use super::{BUTTON_HEIGHT, CHECK_PITCH, FIELD_PITCH, FIRST_FRAME, FIRST_ROW, LINE};

    pub(super) const RECORDING_FRAME: i32 = FIRST_FRAME;
    pub(super) const REPLAY_LENGTH: i32 = RECORDING_FRAME + FIRST_ROW;
    pub(super) const ENCODING: i32 = REPLAY_LENGTH + FIELD_PITCH;
    pub(super) const HOTKEY: i32 = ENCODING + FIELD_PITCH;
    pub(super) const BACKEND: i32 = HOTKEY + FIELD_PITCH;
    pub(super) const FOLDER: i32 = BACKEND + FIELD_PITCH;
    pub(super) const MEMORY: i32 = FOLDER + FIELD_PITCH;
    pub(super) const AUDIO: i32 = MEMORY + FIELD_PITCH;
    pub(super) const CURSOR: i32 = AUDIO + CHECK_PITCH;
    pub(super) const ESTIMATE: i32 = CURSOR + CHECK_PITCH + 4;
    pub(super) const APPLY: i32 = ESTIMATE + LINE;
    pub(super) const RECORDING_HEIGHT: i32 = APPLY + BUTTON_HEIGHT + 14 - RECORDING_FRAME;
    pub(super) const GENERAL_FRAME: i32 = RECORDING_FRAME + RECORDING_HEIGHT;
    pub(super) const THEME: i32 = GENERAL_FRAME + FIRST_ROW;
    pub(super) const START_WITH_WINDOWS: i32 = THEME + FIELD_PITCH;
    pub(super) const STARTUP_ERROR: i32 = START_WITH_WINDOWS + CHECK_PITCH;
    pub(super) const GENERAL_HEIGHT: i32 = START_WITH_WINDOWS + LINE + 6 - GENERAL_FRAME;
}

/// Why the settings entered were not applied. Shown beside Apply.
#[derive(Debug, Error)]
pub(super) enum SettingsError {
    #[error("{}", text::NUMBER_REQUIRED)]
    NumberRequired,
    #[error(transparent)]
    Hotkey(#[from] hotkey::ParseError),
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    #[error(transparent)]
    Register(#[from] RegisterError),
}

#[derive(Clone)]
pub(super) struct SettingsPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    frames: Vec<Frame>,
    general_group: ExpandableGroup,
    recording: RecordingControls,
    general: GeneralControls,
    /// The codecs offered, in the order of the codec list's items.
    codecs: Vec<Codec>,
}

impl Painted for Inner {
    fn frames(&self) -> &[Frame] {
        &self.frames
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        [
            &self.recording.error,
            &self.recording.estimate,
            &self.general.error,
        ]
        .into_iter()
    }
}

impl Pane for SettingsPage {
    fn title(&self) -> &'static str {
        text::TAB_SETTINGS
    }

    fn page(&self) -> &gui::TabPage {
        &self.0.page
    }

    fn on_create(&self) {
        layout::fit_titles(&self.0.frames);
        self.update_estimate();
        self.update_cursor_choice();
    }

    fn frames(&self) -> &[Frame] {
        &self.0.frames
    }
}

impl SettingsPage {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        config: &Config,
        codecs: &[Codec],
        start_with_windows: bool,
        skin: &Skin,
    ) -> Self {
        let page = layout::page(parent);
        let recording_frame = Frame::new(
            &page,
            text::GROUP_RECORDING,
            rows::RECORDING_FRAME,
            rows::RECORDING_HEIGHT,
        );
        let recording = RecordingControls::new(&page, config, codecs, skin);
        let general_frame = Frame::new(
            &page,
            text::GROUP_GENERAL,
            rows::GENERAL_FRAME,
            rows::GENERAL_HEIGHT,
        );
        let general = GeneralControls::new(&page, config, start_with_windows);
        let frames = vec![recording_frame, general_frame];
        let general_group =
            ExpandableGroup::new(1, general.error.label().clone(), STARTUP_ERROR_EXTRA);
        let inner = Rc::new(Inner {
            page,
            frames,
            general_group,
            recording,
            general,
            codecs: codecs.to_vec(),
        });
        pane::paint(&inner.page, skin, Rc::clone(&inner));
        let page = Self(inner);
        page.estimate_events();
        page.browse_events();
        page.backend_events();
        page
    }

    pub(super) fn extra(&self) -> i32 {
        self.0.general_group.extra()
    }

    pub(super) fn on_apply(&self, apply: impl Fn() + 'static) {
        self.0.recording.apply.on().bn_clicked(move || {
            apply();
            Ok(())
        });
    }

    pub(super) fn on_theme_chosen(&self, chosen: impl Fn(Theme) + 'static) {
        let page = self.clone();
        self.0.general.theme.on().cbn_sel_change(move || {
            chosen(page.chosen_theme());
            Ok(())
        });
    }

    /// `resized` is called when the toggle changed the General group's height.
    pub(super) fn on_startup_toggled(&self, resized: impl Fn() + 'static) {
        let page = self.clone();
        self.0.general.start_with_windows.on().bn_clicked(move || {
            if page.toggle_startup() {
                resized();
            }
            Ok(())
        });
    }

    /// Returns the configuration as entered, after validating it.
    pub(super) fn configuration(&self) -> Result<Config, SettingsError> {
        let config = self.entered()?;
        config.validate()?;
        Ok(config)
    }

    /// `None` clears the message.
    pub(super) fn show_settings_error(&self, error: Option<&SettingsError>) {
        let message = error.map(ToString::to_string).unwrap_or_default();
        self.0.recording.error.set_text(&message);
    }

    pub(super) fn show_hotkey(&self, hotkey: &str) {
        layout::set_text(&self.0.recording.hotkey, hotkey);
    }

    /// Reports whether the General group changed height.
    pub(super) fn show_startup_error(&self, message: Option<&str>) -> bool {
        self.0.general_group.set_message(&self.0.frames, message)
    }

    fn estimate_events(&self) {
        let encoding = &self.0.recording.encoding;
        for field in [&encoding.buffer_seconds, &encoding.bitrate] {
            let page = self.clone();
            field.on().en_change(move || {
                page.update_estimate();
                Ok(())
            });
        }
        let page = self.clone();
        self.0.recording.memory.on().cbn_sel_change(move || {
            page.update_estimate();
            Ok(())
        });
    }

    fn backend_events(&self) {
        let page = self.clone();
        self.0.recording.backend.on().cbn_sel_change(move || {
            page.update_cursor_choice();
            Ok(())
        });
    }

    /// DXGI Desktop Duplication cannot draw the cursor, so the cursor choice is disabled while
    /// that backend is selected.
    fn update_cursor_choice(&self) {
        let recording = &self.0.recording;
        let backend = chosen(&recording.backend, &recording.backends);
        recording
            .cursor
            .hwnd()
            .EnableWindow(backend != Backend::Dxgi);
    }

    fn browse_events(&self) {
        let page = self.clone();
        self.0.recording.browse.on().bn_clicked(move || {
            page.browse_folder();
            Ok(())
        });
    }

    /// Opens the shell's folder dialog on the folder entered and puts the choice in the field.
    fn browse_folder(&self) {
        let folder = &self.0.recording.folder;
        let entered = PathBuf::from(folder.text().unwrap_or_default().trim());
        let page_window = self.0.page.hwnd();
        let root = page_window.GetAncestor(co::GA::ROOT);
        let owner = root.as_ref().unwrap_or(page_window);
        match shell::pick_folder(owner, Some(&entered)) {
            Ok(Some(chosen)) => layout::set_text(folder, &chosen.display().to_string()),
            Ok(None) => {}
            Err(error) => log::warning(text::folder_pick_failed(log::chain(&error))),
        }
    }

    fn update_estimate(&self) {
        let recording = &self.0.recording;
        let estimate = number(&recording.encoding.buffer_seconds)
            .zip(number(&recording.encoding.bitrate))
            .and_then(|(buffer_seconds, bitrate_mbps)| {
                estimate_bytes(buffer_seconds, bitrate_mbps).map(|bytes| (bytes, bitrate_mbps))
            });
        let memory = chosen(&recording.memory, &Memory::ALL);
        recording
            .estimate
            .set_text(&estimate.map_or_else(String::new, |(bytes, bitrate_mbps)| {
                text::buffer_estimate(bytes, memory, bitrate_mbps)
            }));
    }

    fn chosen_theme(&self) -> Theme {
        chosen(&self.0.general.theme, &Theme::ALL)
    }

    /// Reports whether the General group changed height.
    fn toggle_startup(&self) -> bool {
        let wanted = self.0.general.start_with_windows.is_checked();
        match startup::set(wanted) {
            Ok(()) => {
                log::info(if wanted {
                    text::STARTUP_ENABLED
                } else {
                    text::STARTUP_DISABLED
                });
                self.show_startup_error(None)
            }
            Err(error) => {
                let message = text::startup_setting_failed(error);
                log::error(&message);
                self.0.general.start_with_windows.set_check(!wanted);
                self.show_startup_error(Some(&message))
            }
        }
    }

    fn entered(&self) -> Result<Config, SettingsError> {
        let required = |field: &gui::Edit| number(field).ok_or(SettingsError::NumberRequired);
        let recording = &self.0.recording;
        let encoding = &recording.encoding;
        let hotkey = recording.hotkey.text().unwrap_or_default();
        let folder = recording.folder.text().unwrap_or_default();
        Ok(Config {
            mode: chosen(&recording.mode, &Mode::ALL),
            backend: chosen(&recording.backend, &recording.backends),
            codec: chosen(&encoding.codec, &self.0.codecs),
            frame_rate: required(&encoding.frame_rate)?,
            buffer_seconds: required(&encoding.buffer_seconds)?,
            bitrate_mbps: required(&encoding.bitrate)?,
            hotkey: hotkey.trim().parse()?,
            clip_directory: stored_folder(folder.trim()),
            audio: recording.audio.is_checked(),
            microphone: recording.microphone.is_checked(),
            cursor: recording.cursor.is_checked(),
            buffer_memory: chosen(&recording.memory, &Memory::ALL),
            theme: chosen(&self.0.general.theme, &Theme::ALL),
        })
    }
}

struct RecordingControls {
    encoding: EncodingFields,
    hotkey: gui::Edit,
    mode: gui::ComboBox,
    backend: gui::ComboBox,
    /// The backends offered, in the order of the backend list's items.
    backends: Vec<Backend>,
    folder: gui::Edit,
    browse: gui::Button,
    memory: gui::ComboBox,
    audio: gui::CheckBox,
    microphone: gui::CheckBox,
    cursor: gui::CheckBox,
    estimate: TintedLabel,
    apply: gui::Button,
    error: TintedLabel,
}

impl RecordingControls {
    fn new(page: &gui::TabPage, config: &Config, codecs: &[Codec], skin: &Skin) -> Self {
        let encoding = EncodingFields::new(page, config, codecs);
        let hotkey = text_field(
            page,
            text::HOTKEY,
            rows::HOTKEY,
            HOTKEY_WIDTH,
            &config.hotkey.to_string(),
        );
        let backends = Backend::offered(config.backend);
        let (mode, backend) = source_fields(page, config, &backends);
        let folder = text_field(
            page,
            text::FOLDER,
            rows::FOLDER,
            FOLDER_WIDTH,
            &config.clip_directory().display().to_string(),
        );
        let browse = button(page, text::BROWSE, BROWSE_X, rows::FOLDER, BROWSE_WIDTH);
        skin_button(&browse, skin);
        let memory = choice_field(
            page,
            text::MEMORY,
            INNER_X,
            rows::MEMORY,
            CHOICE_WIDTH,
            &Memory::ALL.map(Memory::name),
            index_of(&Memory::ALL, config.buffer_memory),
        );
        let audio = check_box(
            page,
            text::AUDIO,
            INNER_X,
            rows::AUDIO,
            COLUMN_X - INNER_X,
            config.audio,
        );
        let microphone = check_box(
            page,
            text::MICROPHONE,
            COLUMN_X,
            rows::AUDIO,
            CHECK_COLUMN_WIDTH,
            config.microphone,
        );
        let cursor = check_box(
            page,
            text::CURSOR,
            INNER_X,
            rows::CURSOR,
            INNER_WIDTH,
            config.cursor,
        );
        let estimate = TintedLabel::new(
            label(page, "", INNER_X, rows::ESTIMATE, INNER_WIDTH, 1),
            Tone::Muted,
        );
        let apply = button(page, text::APPLY, INNER_X, rows::APPLY, BUTTON_WIDTH);
        skin_button(&apply, skin);
        let error = label(
            page,
            &codec_warning(config, codecs),
            INNER_X + ERROR_X,
            rows::APPLY - 2,
            INNER_WIDTH - ERROR_X,
            2,
        );
        let error = TintedLabel::new(error, Tone::Error);
        Self {
            encoding,
            hotkey,
            mode,
            backend,
            backends,
            folder,
            browse,
            memory,
            audio,
            microphone,
            cursor,
            estimate,
            apply,
            error,
        }
    }
}

struct EncodingFields {
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    _spins: [gui::UpDown; 3],
}

impl EncodingFields {
    fn new(page: &gui::TabPage, config: &Config, codecs: &[Codec]) -> Self {
        let (buffer_seconds, buffer_spin) = number_field(
            page,
            text::REPLAY_LENGTH,
            INNER_X,
            rows::REPLAY_LENGTH,
            (MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS),
            config.buffer_seconds,
        );
        let (bitrate, bitrate_spin) = number_field(
            page,
            text::BITRATE,
            COLUMN_X,
            rows::REPLAY_LENGTH,
            (MIN_BITRATE_MBPS, MAX_BITRATE_MBPS),
            config.bitrate_mbps,
        );
        let names = codecs.iter().map(|codec| codec.name()).collect::<Vec<_>>();
        let codec = choice_field(
            page,
            text::CODEC,
            INNER_X,
            rows::ENCODING,
            CODEC_WIDTH,
            &names,
            index_of(codecs, config.codec).or(Some(0)),
        );
        let (frame_rate, frame_rate_spin) = number_field(
            page,
            text::FRAME_RATE,
            COLUMN_X,
            rows::ENCODING,
            (MIN_FRAME_RATE, MAX_FRAME_RATE),
            config.frame_rate,
        );
        Self {
            buffer_seconds,
            bitrate,
            codec,
            frame_rate,
            _spins: [buffer_spin, bitrate_spin, frame_rate_spin],
        }
    }
}

struct GeneralControls {
    theme: gui::ComboBox,
    start_with_windows: gui::CheckBox,
    error: TintedLabel,
}

impl GeneralControls {
    fn new(page: &gui::TabPage, config: &Config, start_with_windows: bool) -> Self {
        let theme = choice_field(
            page,
            text::THEME,
            INNER_X,
            rows::THEME,
            CHOICE_WIDTH,
            &Theme::ALL.map(Theme::name),
            index_of(&Theme::ALL, config.theme),
        );
        let start_with_windows = check_box(
            page,
            text::START_WITH_WINDOWS,
            INNER_X,
            rows::START_WITH_WINDOWS,
            INNER_WIDTH,
            start_with_windows,
        );
        let error = TintedLabel::new(
            hidden_label(page, INNER_X, rows::STARTUP_ERROR, INNER_WIDTH, 1),
            Tone::Error,
        );
        Self {
            theme,
            start_with_windows,
            error,
        }
    }
}

fn choice_field(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    width: i32,
    items: &[&str],
    selected: Option<u32>,
) -> gui::ComboBox {
    field_caption(page, caption, x, y);
    gui::ComboBox::new(
        page,
        gui::ComboBoxOpts {
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(width),
            items,
            selected_item: selected,
            ..Default::default()
        },
    )
}

fn number_field(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    (minimum, maximum): (u32, u32),
    value: u32,
) -> (gui::Edit, gui::UpDown) {
    field_caption(page, caption, x, y);
    let initial = value.to_string();
    let edit = gui::Edit::new(
        page,
        gui::EditOpts {
            text: &initial,
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(NUMBER_WIDTH),
            control_style: co::ES::NUMBER | co::ES::AUTOHSCROLL | co::ES::NOHIDESEL,
            ..Default::default()
        },
    );
    let spin = gui::UpDown::new(
        page,
        gui::UpDownOpts {
            range: (saturating_i32(minimum), saturating_i32(maximum)),
            value: saturating_i32(value),
            // No thousands separators, because the field is parsed back as a plain number.
            control_style: co::UDS::AUTOBUDDY
                | co::UDS::SETBUDDYINT
                | co::UDS::ALIGNRIGHT
                | co::UDS::ARROWKEYS
                | co::UDS::HOTTRACK
                | co::UDS::NOTHOUSANDS,
            ..Default::default()
        },
    );
    (edit, spin)
}

fn check_box(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    width: i32,
    checked: bool,
) -> gui::CheckBox {
    gui::CheckBox::new(
        page,
        gui::CheckBoxOpts {
            text: caption,
            position: gui::dpi(x, y),
            size: gui::dpi(width, LINE),
            check_state: if checked {
                co::BST::CHECKED
            } else {
                co::BST::UNCHECKED
            },
            ..Default::default()
        },
    )
}

fn text_field(page: &gui::TabPage, caption: &str, y: i32, width: i32, value: &str) -> gui::Edit {
    field_caption(page, caption, INNER_X, y);
    gui::Edit::new(
        page,
        gui::EditOpts {
            text: value,
            position: gui::dpi(INNER_X + FIELD_OFFSET, y),
            width: gui::dpi_x(width),
            control_style: co::ES::AUTOHSCROLL | co::ES::NOHIDESEL,
            ..Default::default()
        },
    )
}

fn source_fields(
    page: &gui::TabPage,
    config: &Config,
    backends: &[Backend],
) -> (gui::ComboBox, gui::ComboBox) {
    let mode = choice_field(
        page,
        text::MODE,
        COLUMN_X,
        rows::HOTKEY,
        COLUMN_WIDTH,
        &Mode::ALL.map(Mode::name),
        index_of(&Mode::ALL, config.mode),
    );
    let backend = choice_field(
        page,
        text::BACKEND,
        INNER_X,
        rows::BACKEND,
        FULL_WIDTH,
        &backends
            .iter()
            .map(|backend| backend.name())
            .collect::<Vec<_>>(),
        index_of(backends, config.backend),
    );
    (mode, backend)
}

/// Also logs the warning, because the label is seen only once the tab is.
fn codec_warning(config: &Config, codecs: &[Codec]) -> String {
    if codecs.contains(&config.codec) {
        return String::new();
    }
    let warning = text::codec_unsupported(config.codec.name());
    log::warning(&warning);
    warning
}

fn field_caption(page: &gui::TabPage, caption: &str, x: i32, y: i32) {
    let _caption = label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1);
}

/// Returns `None` for the default folder, so that a default folder that later moves is followed.
fn stored_folder(entered: &str) -> Option<PathBuf> {
    (entered != config::default_clip_directory().display().to_string())
        .then(|| PathBuf::from(entered))
}

/// Returns `None` outside the limits Apply enforces, where the configuration's arithmetic would
/// overflow.
fn estimate_bytes(buffer_seconds: u32, bitrate_mbps: u32) -> Option<u64> {
    let within = (MIN_BUFFER_SECONDS..=MAX_BUFFER_SECONDS).contains(&buffer_seconds)
        && (MIN_BITRATE_MBPS..=MAX_BITRATE_MBPS).contains(&bitrate_mbps);
    within.then(|| {
        Config {
            buffer_seconds,
            bitrate_mbps,
            ..Config::default()
        }
        .estimated_buffer_bytes()
    })
}

fn number(field: &gui::Edit) -> Option<u32> {
    field
        .text()
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

fn chosen<T: Copy + Default>(field: &gui::ComboBox, all: &[T]) -> T {
    field
        .items()
        .selected_index()
        .and_then(|index| all.get(usize::try_from(index).ok()?).copied())
        .unwrap_or_default()
}

fn index_of<T: Copy + PartialEq>(all: &[T], value: T) -> Option<u32> {
    all.iter()
        .position(|item| *item == value)
        .and_then(|index| u32::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_clip_folder_is_stored_as_none_and_any_other_as_entered() {
        let default = config::default_clip_directory().display().to_string();
        assert_eq!(stored_folder(&default), None);
        assert_eq!(stored_folder(r"D:\Clips"), Some(PathBuf::from(r"D:\Clips")));
    }

    #[test]
    fn a_settings_error_reads_as_the_message_it_stands_for() {
        assert_eq!(
            SettingsError::NumberRequired.to_string(),
            text::NUMBER_REQUIRED
        );
        let parse = "".parse::<hotkey::Hotkey>().unwrap_err();
        let shown = parse.to_string();
        assert_eq!(SettingsError::from(parse).to_string(), shown);
        assert_eq!(
            SettingsError::from(ConfigError::Buffer).to_string(),
            ConfigError::Buffer.to_string()
        );
    }

    #[test]
    fn the_rows_sit_where_they_were_laid_by_hand() {
        assert_eq!(
            [
                rows::REPLAY_LENGTH,
                rows::ENCODING,
                rows::HOTKEY,
                rows::BACKEND,
                rows::FOLDER,
                rows::MEMORY,
                rows::AUDIO,
                rows::CURSOR,
                rows::ESTIMATE,
                rows::APPLY,
            ],
            [26, 54, 82, 110, 138, 166, 194, 216, 242, 260]
        );
        assert_eq!((rows::RECORDING_FRAME, rows::RECORDING_HEIGHT), (6, 292));
        assert_eq!((rows::GENERAL_FRAME, rows::GENERAL_HEIGHT), (298, 72));
        assert_eq!(
            rows::GENERAL_FRAME + rows::GENERAL_HEIGHT,
            layout::FILL_Y + layout::FILL_HEIGHT
        );
        assert_eq!(
            [rows::THEME, rows::START_WITH_WINDOWS, rows::STARTUP_ERROR],
            [318, 346, 368]
        );
    }

    #[test]
    fn the_estimate_covers_the_accepted_range_and_nothing_beyond_it() {
        assert_eq!(estimate_bytes(60, 20), Some(150_000_000));
        assert_eq!(
            estimate_bytes(MAX_BUFFER_SECONDS, MAX_BITRATE_MBPS),
            Some(u64::from(MAX_BUFFER_SECONDS) * u64::from(MAX_BITRATE_MBPS) * 1_000_000 / 8)
        );
        assert_eq!(estimate_bytes(60, u32::MAX), None);
        assert_eq!(estimate_bytes(MAX_BUFFER_SECONDS + 1, 20), None);
        assert_eq!(estimate_bytes(60, MIN_BITRATE_MBPS - 1), None);
    }
}
