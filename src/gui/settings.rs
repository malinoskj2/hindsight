//! The Settings tab: what to record and how, codec, frame rate, buffer length, bitrate, sound, the
//! save hotkey and clip folder, the theme and the Start-with-Windows entry.

use super::{
    layout::{
        self, BLACK, BUTTON_WIDTH, ExpandableGroup, Frame, GRAY, INNER_WIDTH, INNER_X, LINE, RED,
        button, label, skin_button,
    },
    text,
    theme::{ButtonRole, Skin, Surface},
};
use crate::{
    config::{
        self, Backend, Codec, Config, MAX_BITRATE_MBPS, MAX_BUFFER_SECONDS, MAX_FRAME_RATE,
        MIN_BITRATE_MBPS, MIN_BUFFER_SECONDS, MIN_FRAME_RATE, Mode, Theme,
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
    audio: gui::CheckBox,
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
            audio: recording.audio,
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

    /// Shows how much memory the buffer takes at the entered bitrate and length.
    pub(super) fn update_estimate(&self) {
        let estimate = number(&self.buffer_seconds).zip(number(&self.bitrate)).map(
            |(buffer_seconds, bitrate_mbps)| {
                Config {
                    buffer_seconds,
                    bitrate_mbps,
                    ..Config::default()
                }
                .estimated_buffer_bytes()
                    / BYTES_PER_MEGABYTE
            },
        );
        layout::set_text(
            &self.estimate,
            &estimate.map_or_else(String::new, text::buffer_estimate),
        );
    }

    /// The fields whose changes alter the memory estimate.
    pub(super) fn estimate_inputs(&self) -> [&gui::Edit; 2] {
        [&self.buffer_seconds, &self.bitrate]
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
            theme: chosen(&self.theme, &Theme::ALL),
        })
    }
}

/// The Recording group: the encoding, source and clip fields, the sound switch, the memory
/// estimate, and the Apply button with the line that reports a problem with them.
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
    audio: gui::CheckBox,
    estimate: gui::Label,
    apply: gui::Button,
    error: gui::Label,
}

impl RecordingControls {
    /// Lays out the Recording group on `page`, filled in from `config`, its button in the look
    /// of `skin`.
    fn new(page: &gui::TabPage, config: &Config, skin: &Skin) -> Self {
        let frame = Frame::new(page, text::GROUP_RECORDING_SETTINGS, 6, 242);
        let codec = choice_field(
            page,
            text::CODEC,
            INNER_X,
            26,
            90,
            &Codec::ALL.map(Codec::name),
            index_of(&Codec::ALL, config.codec),
        );
        let (frame_rate, frame_rate_spin) = number_field(
            page,
            text::FRAME_RATE,
            COLUMN_X,
            26,
            (MIN_FRAME_RATE, MAX_FRAME_RATE),
            config.frame_rate,
        );
        let (buffer_seconds, buffer_spin) = number_field(
            page,
            text::BUFFER_SECONDS,
            INNER_X,
            54,
            (MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS),
            config.buffer_seconds,
        );
        let (bitrate, bitrate_spin) = number_field(
            page,
            text::BITRATE,
            COLUMN_X,
            54,
            (MIN_BITRATE_MBPS, MAX_BITRATE_MBPS),
            config.bitrate_mbps,
        );
        let hotkey = text_field(page, text::HOTKEY, 82, 110, &config.hotkey);
        let (mode, backend) = source_fields(page, config);
        let folder = text_field(
            page,
            text::FOLDER,
            138,
            INNER_WIDTH - FIELD_OFFSET,
            &config.clip_directory().display().to_string(),
        );
        let audio = check_box(page, text::AUDIO, 166, config.audio);
        let estimate = label(page, "", INNER_X, 192, INNER_WIDTH, 1);
        let apply = button(page, text::APPLY, INNER_X, 210, BUTTON_WIDTH);
        skin_button(&apply, skin, ButtonRole::Primary);
        // The error line starts just right of the button.
        let error_x = BUTTON_WIDTH + 8;
        let error = label(page, "", INNER_X + error_x, 208, INNER_WIDTH - error_x, 2);
        Self {
            frame,
            codec,
            frame_rate,
            buffer_seconds,
            bitrate,
            spins: [frame_rate_spin, buffer_spin, bitrate_spin],
            hotkey,
            mode,
            backend,
            folder,
            audio,
            estimate,
            apply,
            error,
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
        let frame = Frame::new(page, text::GROUP_GENERAL, 248, 72);
        let theme = choice_field(
            page,
            text::THEME,
            INNER_X,
            268,
            190,
            &Theme::ALL.map(Theme::name),
            index_of(&Theme::ALL, config.theme),
        );
        let check_box = check_box(page, text::START_WITH_WINDOWS, 296, start_with_windows);
        let error = label(page, "", INNER_X, 318, INNER_WIDTH, 1);
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
        82,
        INNER_WIDTH - (COLUMN_X - INNER_X) - FIELD_OFFSET,
        &Mode::ALL.map(Mode::name),
        index_of(&Mode::ALL, config.mode),
    );
    let backend = choice_field(
        page,
        text::BACKEND,
        INNER_X,
        110,
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
}
