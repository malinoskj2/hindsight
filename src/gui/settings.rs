//! The Settings tab: what to record and how, codec, frame rate, buffer length, bitrate, sound, the
//! save hotkey and clip folder, the theme and the Start-with-Windows entry.

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
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

/// Height the General group gains while it shows a Run-key error line.
const STARTUP_ERROR_EXTRA: i32 = LINE + 4;
/// Where the second column of fields starts.
const COLUMN_X: i32 = INNER_X + 204;
/// How far a field sits to the right of the start of its caption.
const FIELD_OFFSET: i32 = 88;
/// Width of a numeric field, spin buttons included.
const NUMBER_WIDTH: i32 = 52;
/// Bytes in a megabyte, as the memory estimate counts them.
const BYTES_PER_MEGABYTE: u64 = 1_000_000;
/// How far below the top of a group's frame its first row sits.
const FIRST_ROW: i32 = 20;
/// Pitch of the rows of fields.
const FIELD_PITCH: i32 = 28;
/// Pitch of the rows of check boxes.
const CHECK_PITCH: i32 = LINE + 4;

/// Where each row of the page sits, top to bottom. Each is placed below the one before, so a
/// row added or removed moves the rest; the tests pin the pixels this comes to.
mod rows {
    use super::{BUTTON_HEIGHT, CHECK_PITCH, FIELD_PITCH, FIRST_ROW, LINE};

    /// Top of the Recording group's frame.
    pub(super) const RECORDING_FRAME: i32 = 6;
    /// Codec and frame rate.
    pub(super) const ENCODING: i32 = RECORDING_FRAME + FIRST_ROW;
    /// Buffer length and bitrate.
    pub(super) const BUFFER: i32 = ENCODING + FIELD_PITCH;
    /// The save hotkey and what to record.
    pub(super) const HOTKEY: i32 = BUFFER + FIELD_PITCH;
    pub(super) const BACKEND: i32 = HOTKEY + FIELD_PITCH;
    pub(super) const FOLDER: i32 = BACKEND + FIELD_PITCH;
    pub(super) const MEMORY: i32 = FOLDER + FIELD_PITCH;
    pub(super) const AUDIO: i32 = MEMORY + FIELD_PITCH;
    pub(super) const CURSOR: i32 = AUDIO + CHECK_PITCH;
    /// The memory estimate, a little clear of the switches.
    pub(super) const ESTIMATE: i32 = CURSOR + CHECK_PITCH + 4;
    pub(super) const APPLY: i32 = ESTIMATE + LINE;
    /// Down to the Apply button, with room under it.
    pub(super) const RECORDING_HEIGHT: i32 = APPLY + BUTTON_HEIGHT + 14 - RECORDING_FRAME;
    /// Top of the General group's frame, right under the Recording group's.
    pub(super) const GENERAL_FRAME: i32 = RECORDING_FRAME + RECORDING_HEIGHT;
    pub(super) const THEME: i32 = GENERAL_FRAME + FIRST_ROW;
    pub(super) const START_WITH_WINDOWS: i32 = THEME + FIELD_PITCH;
    pub(super) const STARTUP_ERROR: i32 = START_WITH_WINDOWS + CHECK_PITCH;
    /// Compact, down to the check box; the error line is made room for only while it shows.
    pub(super) const GENERAL_HEIGHT: i32 = START_WITH_WINDOWS + LINE + 6 - GENERAL_FRAME;
}

#[derive(Clone)]
pub(super) struct SettingsPage {
    page: gui::TabPage,
    frames: Vec<Frame>,
    /// The General group; it grows while the Run-key error label has something to show.
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
    /// The capture choice last settled on, restored when the NvFBC key is not given.
    settled_backend: Rc<Cell<Backend>>,
}

impl SettingsPage {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        config: &Config,
        start_with_windows: bool,
        skin: &Skin,
    ) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let recording = RecordingControls::new(&page, config, skin);
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
            settled_backend: Rc::new(Cell::new(config.backend)),
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
        self.general_group.on_create();
        self.update_estimate();
    }

    /// The extra height the page currently needs for its expanded group, in logical pixels.
    pub(super) fn extra(&self) -> i32 {
        self.general_group.extra()
    }

    /// The configuration as currently entered, or the problem with it.
    pub(super) fn configuration(&self) -> Result<Config, String> {
        validated(self.entered()?)
    }

    /// The capture backend the drop-down list shows.
    pub(super) fn chosen_backend(&self) -> Backend {
        chosen(&self.backend, &Backend::ALL)
    }

    /// The look the drop-down list shows.
    pub(super) fn chosen_theme(&self) -> Theme {
        chosen(&self.theme, &Theme::ALL)
    }

    /// Accepts the backend now shown, or, when `accepted` is false, shows the previous one
    /// again.
    pub(super) fn settle_backend(&self, accepted: bool) {
        if accepted {
            self.settled_backend.set(self.chosen_backend());
        } else {
            let previous = index_of(&Backend::ALL, self.settled_backend.get());
            self.backend.items().select(previous);
        }
    }

    /// Shows the problem with the entered settings, or clears it.
    pub(super) fn show_settings_error(&self, message: &str) {
        layout::set_text(&self.settings_error, message);
    }

    /// Shows the canonical spelling of the applied hotkey.
    pub(super) fn show_hotkey(&self, hotkey: &str) {
        layout::set_text(&self.hotkey, hotkey);
    }

    /// Wires the fields and the drop-down list the memory estimate follows.
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

    /// Shows how much memory the buffer takes at the entered bitrate and length, and where;
    /// nothing while either is not a number Apply would accept.
    pub(super) fn update_estimate(&self) {
        let estimate = number(&self.buffer_seconds)
            .zip(number(&self.bitrate))
            .and_then(|(buffer_seconds, bitrate_mbps)| {
                estimate_megabytes(buffer_seconds, bitrate_mbps)
            });
        let memory = chosen(&self.memory, &Memory::ALL);
        layout::set_text(
            &self.estimate,
            &estimate.map_or_else(String::new, |megabytes| {
                text::buffer_estimate(megabytes, memory)
            }),
        );
    }

    /// Writes the Start-with-Windows choice to the Run key, undoing the tick when that fails,
    /// and reports whether the General group changed height.
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

    /// The General group stays compact until a Run-key error needs its extra line; reports
    /// whether its height changed, so the caller refits the window.
    pub(super) fn show_startup_error(&self, message: Option<&str>) -> bool {
        self.general_group.set_message(message)
    }

    /// The fields read back as a configuration, not yet validated, or the problem when a
    /// numeric field does not hold a whole number.
    fn entered(&self) -> Result<Config, String> {
        let required =
            |field: &gui::Edit| number(field).ok_or_else(|| text::NUMBER_REQUIRED.to_owned());
        let hotkey = self.hotkey.text().unwrap_or_default();
        let folder = self.folder.text().unwrap_or_default();
        Ok(Config {
            mode: chosen(&self.mode, &Mode::ALL),
            backend: chosen(&self.backend, &Backend::ALL),
            codec: chosen(&self.codec, &Codec::ALL),
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

/// The Recording group: the encoding, source and clip fields, the sound and cursor switches,
/// the memory estimate, and the Apply button with the line that reports a problem with them.
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
    /// Lays out the Recording group on `page`, filled in from `config`, its button in the look
    /// of `skin`.
    fn new(page: &gui::TabPage, config: &Config, skin: &Skin) -> Self {
        let frame = Frame::new(
            page,
            text::GROUP_RECORDING_SETTINGS,
            rows::RECORDING_FRAME,
            rows::RECORDING_HEIGHT,
        );
        let encoding = EncodingFields::new(page, config);
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
        // The error line starts just right of the button, a little higher so its two lines
        // sit level with it.
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

/// The first two rows of the Recording group: the codec and frame rate, then the buffer
/// length and bitrate, each number with its spin buttons.
struct EncodingFields {
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    spins: [gui::UpDown; 3],
}

impl EncodingFields {
    /// Lays out the encoding fields on `page`, filled in from `config`.
    fn new(page: &gui::TabPage, config: &Config) -> Self {
        let codec = choice_field(
            page,
            text::CODEC,
            INNER_X,
            rows::ENCODING,
            90,
            &Codec::ALL.map(Codec::name),
            index_of(&Codec::ALL, config.codec),
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

/// The General group: the theme, and the Start-with-Windows entry with the line that reports a
/// Run-key error.
struct GeneralControls {
    frame: Frame,
    theme: gui::ComboBox,
    check_box: gui::CheckBox,
    error: gui::Label,
    group: ExpandableGroup,
}

impl GeneralControls {
    /// Lays out the General group on `page`, filled in from `config` and the Run key's state.
    fn new(page: &gui::TabPage, config: &Config, start_with_windows: bool) -> Self {
        // Compact; the group adds a line for the error label when one occurs.
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

/// The colour of each label on the page: the `errors` lines are red and the `muted` estimate
/// gray; every other label is black.
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

/// A caption and a drop-down list of `items`, with the one at `selected` chosen.
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

/// A caption, a numeric edit and its spin buttons.
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

/// A check box across the group at height `y`, ticked when `checked`.
fn check_box(page: &gui::TabPage, caption: &str, y: i32, checked: bool) -> gui::CheckBox {
    gui::CheckBox::new(
        page,
        gui::CheckBoxOpts {
            text: caption,
            position: gui::dpi(INNER_X, y),
            // Wide enough for the label in any font, rather than measured once at creation.
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

/// A caption and a free-text edit.
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

/// The drop-down lists of what is recorded and how it is captured.
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

/// A field's caption, at the left of the field it names.
fn field_caption(page: &gui::TabPage, caption: &str, x: i32, y: i32) {
    // The caption sits a little lower than the field so their text lines up.
    let _caption = label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1);
}

/// `config` once it passes validation, with its hotkey in the canonical spelling the Status tab
/// shows, or the problem with it.
fn validated(mut config: Config) -> Result<Config, String> {
    config.validate().map_err(|error| error.to_string())?;
    config.hotkey = config
        .parsed_hotkey()
        .map_err(|error| error.to_string())?
        .to_string();
    Ok(config)
}

/// The clip folder `entered` as the configuration stores it: empty for the default folder, so
/// that it follows the default if that moves.
fn stored_folder(entered: &str) -> String {
    if entered == config::default_clip_directory().display().to_string() {
        String::new()
    } else {
        entered.to_owned()
    }
}

/// The buffer's size in megabytes at `buffer_seconds` of `bitrate_mbps`, as the configuration
/// counts it. `None` outside the limits Apply enforces, where the figure would mean nothing
/// and the configuration's arithmetic would overflow.
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

/// The whole number in `field`, if it holds one.
fn number(field: &gui::Edit) -> Option<u32> {
    field
        .text()
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

/// The choice a drop-down list shows, out of `all`, or the first when none is selected.
fn chosen<T: Copy + Default>(field: &gui::ComboBox, all: &[T]) -> T {
    field
        .items()
        .selected_index()
        .and_then(|index| all.get(index as usize).copied())
        .unwrap_or_default()
}

/// Where `value` stands in `all`, as a drop-down list counts its items.
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
            [rows::THEME, rows::START_WITH_WINDOWS, rows::STARTUP_ERROR],
            [318, 346, 368]
        );
    }

    #[test]
    fn the_estimate_covers_the_accepted_range_and_nothing_beyond_it() {
        // 60 seconds at 20 Mbps: 150 MB of encoded video.
        assert_eq!(estimate_megabytes(60, 20), Some(150));
        assert_eq!(
            estimate_megabytes(MAX_BUFFER_SECONDS, MAX_BITRATE_MBPS),
            Some(u64::from(MAX_BUFFER_SECONDS) * u64::from(MAX_BITRATE_MBPS) / 8)
        );
        // Beyond the limits the configuration's own arithmetic would overflow.
        assert_eq!(estimate_megabytes(60, u32::MAX), None);
        assert_eq!(estimate_megabytes(MAX_BUFFER_SECONDS + 1, 20), None);
        assert_eq!(estimate_megabytes(60, MIN_BITRATE_MBPS - 1), None);
    }
}
