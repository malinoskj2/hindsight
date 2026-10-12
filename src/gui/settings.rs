//! The Settings tab.

use super::{
    layout::{
        self, BLACK, BUTTON_HEIGHT, BUTTON_WIDTH, ExpandableGroup, Frame, GRAY, INNER_WIDTH,
        INNER_X, LINE, RED, button, label, skin_button,
    },
    text,
    theme::{Skin, Surface},
};
use crate::{
    config::{
        self, Backend, Codec, Config, MAX_BITRATE_MBPS, MAX_BUFFER_SECONDS, MAX_FRAME_RATE,
        MIN_BITRATE_MBPS, MIN_BUFFER_SECONDS, MIN_FRAME_RATE, Memory, Mode, Theme,
    },
    log,
    sys::startup,
};
use winsafe::{self as w, co, gui, prelude::*};

const STARTUP_ERROR_EXTRA: i32 = LINE + 4;
const COLUMN_X: i32 = INNER_X + 204;
const FIELD_OFFSET: i32 = 88;
const NUMBER_WIDTH: i32 = 52;
const BYTES_PER_MEGABYTE: u64 = 1_000_000;
const FIRST_ROW: i32 = 20;
const FIELD_PITCH: i32 = 28;
const CHECK_PITCH: i32 = LINE + 4;

mod rows {
    use super::{BUTTON_HEIGHT, CHECK_PITCH, FIELD_PITCH, FIRST_ROW, LINE};

    pub(super) const RECORDING_FRAME: i32 = 6;
    pub(super) const ENCODING: i32 = RECORDING_FRAME + FIRST_ROW;
    pub(super) const BUFFER: i32 = ENCODING + FIELD_PITCH;
    pub(super) const HOTKEY: i32 = BUFFER + FIELD_PITCH;
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

#[derive(Clone)]
pub(super) struct SettingsPage {
    page: gui::TabPage,
    frames: Vec<Frame>,
    general_group: ExpandableGroup,
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    _spins: [gui::UpDown; 3],
    hotkey: gui::Edit,
    mode: gui::ComboBox,
    pub(super) backend: gui::ComboBox,
    folder: gui::Edit,
    memory: gui::ComboBox,
    audio: gui::CheckBox,
    cursor: gui::CheckBox,
    estimate: gui::Label,
    pub(super) apply: gui::Button,
    settings_error: gui::Label,
    pub(super) theme: gui::ComboBox,
    pub(super) start_with_windows: gui::CheckBox,
    /// The codecs offered, in the order of the codec list's items.
    codecs: Vec<Codec>,
}

impl SettingsPage {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        config: &Config,
        codecs: &[Codec],
        start_with_windows: bool,
        skin: &Skin,
    ) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let recording = RecordingControls::new(&page, config, codecs, skin);
        let general = GeneralControls::new(&page, config, start_with_windows);
        let frames = vec![recording.frame, general.frame];
        layout::draw(
            &page,
            page.on(),
            frames.clone(),
            skin,
            Surface::Page,
            label_colours(
                [recording.error.clone(), general.error],
                recording.estimate.clone(),
            ),
        );

        Self {
            page,
            frames,
            general_group: general.group,
            codec: recording.codec,
            frame_rate: recording.frame_rate,
            buffer_seconds: recording.buffer_seconds,
            bitrate: recording.bitrate,
            _spins: recording.spins,
            hotkey: recording.hotkey,
            mode: recording.mode,
            backend: recording.backend,
            folder: recording.folder,
            memory: recording.memory,
            audio: recording.audio,
            cursor: recording.cursor,
            estimate: recording.estimate,
            apply: recording.apply,
            settings_error: recording.error,
            theme: general.theme,
            start_with_windows: general.check_box,
            codecs: codecs.to_vec(),
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
        self.general_group.on_create();
        self.update_estimate();
    }

    pub(super) fn extra(&self) -> i32 {
        self.general_group.extra()
    }

    pub(super) fn configuration(&self) -> Result<Config, String> {
        validated(self.entered()?)
    }

    pub(super) fn chosen_theme(&self) -> Theme {
        chosen(&self.theme, &Theme::ALL)
    }

    pub(super) fn show_settings_error(&self, message: &str) {
        layout::set_text(&self.settings_error, message);
    }

    pub(super) fn show_hotkey(&self, hotkey: &str) {
        layout::set_text(&self.hotkey, hotkey);
    }

    pub(super) fn estimate_events(&self) {
        for field in [&self.buffer_seconds, &self.bitrate] {
            let page = self.clone();
            field.on().en_change(move || {
                page.update_estimate();
                Ok(())
            });
        }
        let page = self.clone();
        self.memory.on().cbn_sel_change(move || {
            page.update_estimate();
            Ok(())
        });
    }

    pub(super) fn update_estimate(&self) {
        let estimate = number(&self.buffer_seconds)
            .zip(number(&self.bitrate))
            .and_then(|(buffer_seconds, bitrate_mbps)| {
                estimate_megabytes(buffer_seconds, bitrate_mbps)
                    .map(|megabytes| (megabytes, bitrate_mbps))
            });
        let memory = chosen(&self.memory, &Memory::ALL);
        layout::set_text(
            &self.estimate,
            &estimate.map_or_else(String::new, |(megabytes, bitrate_mbps)| {
                text::buffer_estimate(megabytes, memory, bitrate_mbps)
            }),
        );
    }

    /// Reports whether the General group changed height.
    pub(super) fn toggle_startup(&self) -> bool {
        let wanted = self.start_with_windows.is_checked();
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
                self.start_with_windows.set_check(!wanted);
                self.show_startup_error(Some(&message))
            }
        }
    }

    /// Reports whether the General group changed height.
    pub(super) fn show_startup_error(&self, message: Option<&str>) -> bool {
        self.general_group.set_message(message)
    }

    fn entered(&self) -> Result<Config, String> {
        let required =
            |field: &gui::Edit| number(field).ok_or_else(|| text::NUMBER_REQUIRED.to_owned());
        let hotkey = self.hotkey.text().unwrap_or_default();
        let folder = self.folder.text().unwrap_or_default();
        Ok(Config {
            mode: chosen(&self.mode, &Mode::ALL),
            backend: chosen(&self.backend, &Backend::ALL),
            codec: chosen(&self.codec, &self.codecs),
            frame_rate: required(&self.frame_rate)?,
            buffer_seconds: required(&self.buffer_seconds)?,
            bitrate_mbps: required(&self.bitrate)?,
            hotkey: hotkey.trim().to_owned(),
            clip_directory: stored_folder(folder.trim()),
            audio: self.audio.is_checked(),
            cursor: self.cursor.is_checked(),
            buffer_memory: chosen(&self.memory, &Memory::ALL),
            theme: chosen(&self.theme, &Theme::ALL),
        })
    }
}

struct RecordingControls {
    frame: Frame,
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    spins: [gui::UpDown; 3],
    hotkey: gui::Edit,
    mode: gui::ComboBox,
    backend: gui::ComboBox,
    folder: gui::Edit,
    memory: gui::ComboBox,
    audio: gui::CheckBox,
    cursor: gui::CheckBox,
    estimate: gui::Label,
    apply: gui::Button,
    error: gui::Label,
}

impl RecordingControls {
    fn new(page: &gui::TabPage, config: &Config, codecs: &[Codec], skin: &Skin) -> Self {
        let frame = Frame::new(
            page,
            text::GROUP_RECORDING_SETTINGS,
            rows::RECORDING_FRAME,
            rows::RECORDING_HEIGHT,
        );
        let encoding = EncodingFields::new(page, config, codecs);
        let hotkey = text_field(page, text::HOTKEY, rows::HOTKEY, 110, &config.hotkey);
        let (mode, backend) = source_fields(page, config);
        let folder = text_field(
            page,
            text::FOLDER,
            rows::FOLDER,
            INNER_WIDTH - FIELD_OFFSET,
            &config.clip_directory().display().to_string(),
        );
        let memory = choice_field(
            page,
            text::MEMORY,
            INNER_X,
            rows::MEMORY,
            190,
            &Memory::ALL.map(Memory::name),
            index_of(&Memory::ALL, config.buffer_memory),
        );
        let audio = check_box(page, text::AUDIO, rows::AUDIO, config.audio);
        let cursor = check_box(page, text::CURSOR, rows::CURSOR, config.cursor);
        let estimate = label(page, "", INNER_X, rows::ESTIMATE, INNER_WIDTH, 1);
        let apply = button(page, text::APPLY, INNER_X, rows::APPLY, BUTTON_WIDTH);
        skin_button(&apply, skin);
        let error_x = BUTTON_WIDTH + 8;
        let error = label(
            page,
            "",
            INNER_X + error_x,
            rows::APPLY - 2,
            INNER_WIDTH - error_x,
            2,
        );
        Self {
            frame,
            codec: encoding.codec,
            frame_rate: encoding.frame_rate,
            buffer_seconds: encoding.buffer_seconds,
            bitrate: encoding.bitrate,
            spins: encoding.spins,
            hotkey,
            mode,
            backend,
            folder,
            memory,
            audio,
            cursor,
            estimate,
            apply,
            error,
        }
    }
}

struct EncodingFields {
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    spins: [gui::UpDown; 3],
}

impl EncodingFields {
    fn new(page: &gui::TabPage, config: &Config, codecs: &[Codec]) -> Self {
        let names = codecs.iter().map(|codec| codec.name()).collect::<Vec<_>>();
        let codec = choice_field(
            page,
            text::CODEC,
            INNER_X,
            rows::ENCODING,
            90,
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
        let (buffer_seconds, buffer_spin) = number_field(
            page,
            text::BUFFER_SECONDS,
            INNER_X,
            rows::BUFFER,
            (MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS),
            config.buffer_seconds,
        );
        let (bitrate, bitrate_spin) = number_field(
            page,
            text::BITRATE,
            COLUMN_X,
            rows::BUFFER,
            (MIN_BITRATE_MBPS, MAX_BITRATE_MBPS),
            config.bitrate_mbps,
        );
        Self {
            codec,
            frame_rate,
            buffer_seconds,
            bitrate,
            spins: [frame_rate_spin, buffer_spin, bitrate_spin],
        }
    }
}

struct GeneralControls {
    frame: Frame,
    theme: gui::ComboBox,
    check_box: gui::CheckBox,
    error: gui::Label,
    group: ExpandableGroup,
}

impl GeneralControls {
    fn new(page: &gui::TabPage, config: &Config, start_with_windows: bool) -> Self {
        let frame = Frame::new(
            page,
            text::GROUP_GENERAL,
            rows::GENERAL_FRAME,
            rows::GENERAL_HEIGHT,
        );
        let theme = choice_field(
            page,
            text::THEME,
            INNER_X,
            rows::THEME,
            190,
            &Theme::ALL.map(Theme::name),
            index_of(&Theme::ALL, config.theme),
        );
        let check_box = check_box(
            page,
            text::START_WITH_WINDOWS,
            rows::START_WITH_WINDOWS,
            start_with_windows,
        );
        let error = label(page, "", INNER_X, rows::STARTUP_ERROR, INNER_WIDTH, 1);
        let group = ExpandableGroup::new(frame.clone(), error.clone(), STARTUP_ERROR_EXTRA);
        Self {
            frame,
            theme,
            check_box,
            error,
            group,
        }
    }
}

fn label_colours(
    errors: [gui::Label; 2],
    muted: gui::Label,
) -> impl Fn(&w::HWND) -> w::COLORREF + 'static {
    move |hwnd| {
        if errors.iter().any(|error| *hwnd == *error.hwnd()) {
            RED
        } else if *hwnd == *muted.hwnd() {
            GRAY
        } else {
            BLACK
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
            range: (
                i32::try_from(minimum).unwrap_or(i32::MAX),
                i32::try_from(maximum).unwrap_or(i32::MAX),
            ),
            value: i32::try_from(value).unwrap_or(i32::MAX),
            // No thousands separators: the field is parsed back as a plain number.
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

fn check_box(page: &gui::TabPage, caption: &str, y: i32, checked: bool) -> gui::CheckBox {
    gui::CheckBox::new(
        page,
        gui::CheckBoxOpts {
            text: caption,
            position: gui::dpi(INNER_X, y),
            size: gui::dpi(INNER_WIDTH, LINE),
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

fn source_fields(page: &gui::TabPage, config: &Config) -> (gui::ComboBox, gui::ComboBox) {
    let mode = choice_field(
        page,
        text::MODE,
        COLUMN_X,
        rows::HOTKEY,
        INNER_WIDTH - (COLUMN_X - INNER_X) - FIELD_OFFSET,
        &Mode::ALL.map(Mode::name),
        index_of(&Mode::ALL, config.mode),
    );
    let backend = choice_field(
        page,
        text::BACKEND,
        INNER_X,
        rows::BACKEND,
        INNER_WIDTH - FIELD_OFFSET,
        &Backend::ALL.map(Backend::name),
        index_of(&Backend::ALL, config.backend),
    );
    (mode, backend)
}

fn field_caption(page: &gui::TabPage, caption: &str, x: i32, y: i32) {
    let _caption = label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1);
}

fn validated(mut config: Config) -> Result<Config, String> {
    config.validate().map_err(|error| error.to_string())?;
    config.hotkey = config
        .parsed_hotkey()
        .map_err(|error| error.to_string())?
        .to_string();
    Ok(config)
}

/// Empty for the default folder, so a moved default is followed.
fn stored_folder(entered: &str) -> String {
    if entered == config::default_clip_directory().display().to_string() {
        String::new()
    } else {
        entered.to_owned()
    }
}

/// `None` outside the limits Apply enforces, where the configuration's arithmetic overflows.
fn estimate_megabytes(buffer_seconds: u32, bitrate_mbps: u32) -> Option<u64> {
    let within = (MIN_BUFFER_SECONDS..=MAX_BUFFER_SECONDS).contains(&buffer_seconds)
        && (MIN_BITRATE_MBPS..=MAX_BITRATE_MBPS).contains(&bitrate_mbps);
    within.then(|| {
        Config {
            buffer_seconds,
            bitrate_mbps,
            ..Config::default()
        }
        .estimated_buffer_bytes()
            / BYTES_PER_MEGABYTE
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
        .and_then(|index| all.get(index as usize).copied())
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
    fn the_default_clip_folder_is_stored_as_empty_and_any_other_as_entered() {
        let default = config::default_clip_directory().display().to_string();
        assert_eq!(stored_folder(&default), "");
        assert_eq!(stored_folder(r"D:\Clips"), r"D:\Clips");
    }

    #[test]
    fn the_rows_sit_where_they_were_laid_by_hand() {
        assert_eq!(
            [
                rows::ENCODING,
                rows::BUFFER,
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
        assert_eq!(estimate_megabytes(60, 20), Some(150));
        assert_eq!(
            estimate_megabytes(MAX_BUFFER_SECONDS, MAX_BITRATE_MBPS),
            Some(u64::from(MAX_BUFFER_SECONDS) * u64::from(MAX_BITRATE_MBPS) / 8)
        );
        assert_eq!(estimate_megabytes(60, u32::MAX), None);
        assert_eq!(estimate_megabytes(MAX_BUFFER_SECONDS + 1, 20), None);
        assert_eq!(estimate_megabytes(60, MIN_BITRATE_MBPS - 1), None);
    }
}
