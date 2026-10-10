//! The Settings tab: what to record and how, codec, frame rate, buffer length, bitrate, the save hotkey and clip folder,
//! and the Start-with-Windows entry.

use super::{
    layout::{
        self, BLACK, ExpandableGroup, Frame, GRAY, INNER_WIDTH, INNER_X, LINE, RED, button, label,
    },
    text,
};
use crate::{
    config::{
        self, Backend, Codec, Config, MAX_BITRATE_MBPS, MAX_BUFFER_SECONDS, MAX_FRAME_RATE,
        MIN_BITRATE_MBPS, MIN_BUFFER_SECONDS, MIN_FRAME_RATE, Mode,
    },
    log,
    sys::startup,
};
use winsafe::{co, gui, prelude::*};

/// Height the Startup group gains while it shows a Run-key error line.
const STARTUP_ERROR_EXTRA: i32 = LINE + 4;
/// Where the second column of fields starts.
const COLUMN_X: i32 = INNER_X + 204;
const FIELD_OFFSET: i32 = 88;

#[derive(Clone)]
pub(super) struct SettingsPage {
    page: gui::TabPage,
    frames: Vec<Frame>,
    mode: gui::ComboBox,
    backend: gui::ComboBox,
    codec: gui::ComboBox,
    frame_rate: gui::Edit,
    buffer_seconds: gui::Edit,
    bitrate: gui::Edit,
    _spins: [gui::UpDown; 3],
    hotkey: gui::Edit,
    folder: gui::Edit,
    estimate: gui::Label,
    pub(super) apply: gui::Button,
    settings_error: gui::Label,
    pub(super) start_with_windows: gui::CheckBox,
    /// The Startup group; it grows while the error label has something to show.
    startup: ExpandableGroup,
}

impl SettingsPage {
    pub(super) fn new(
        parent: &(impl GuiParent + 'static),
        config: &Config,
        start_with_windows: bool,
    ) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());

        let recording_group = Frame::new(&page, text::GROUP_RECORDING_SETTINGS, 6, 228);
        let codec = choice_field(
            &page,
            text::CODEC,
            INNER_X,
            26,
            90,
            &Codec::ALL.map(Codec::name),
            Codec::ALL.iter().position(|codec| *codec == config.codec),
        );
        let (frame_rate, frame_rate_spin) = number_field(
            &page,
            text::FRAME_RATE,
            COLUMN_X,
            26,
            (MIN_FRAME_RATE, MAX_FRAME_RATE),
            config.frame_rate,
        );
        let (buffer_seconds, buffer_spin) = number_field(
            &page,
            text::BUFFER_SECONDS,
            INNER_X,
            54,
            (MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS),
            config.buffer_seconds,
        );
        let (bitrate, bitrate_spin) = number_field(
            &page,
            text::BITRATE,
            COLUMN_X,
            54,
            (MIN_BITRATE_MBPS, MAX_BITRATE_MBPS),
            config.bitrate_mbps,
        );
        let hotkey = text_field(&page, text::HOTKEY, 82, 110, &config.hotkey);
        let (mode, backend) = source_fields(&page, config);
        let folder = text_field(
            &page,
            text::FOLDER,
            138,
            INNER_WIDTH - FIELD_OFFSET,
            &config.clip_directory().display().to_string(),
        );
        let estimate = label(&page, "", INNER_X, 164, INNER_WIDTH, 1);
        let _restart_notice = label(&page, text::RESTART_NOTICE, INNER_X, 182, INNER_WIDTH, 1);
        let apply = button(&page, text::APPLY_AND_RESTART, INNER_X, 202, 120);
        let settings_error = label(&page, "", INNER_X + 128, 200, INNER_WIDTH - 128, 2);

        let startup = startup_controls(&page, start_with_windows);
        let frames = vec![recording_group, startup.frame];

        // The error lines are red and the estimate muted; every other label is black.
        let errors = [settings_error.clone(), startup.error];
        let muted = estimate.clone();
        layout::draw(
            &page,
            page.on(),
            frames.clone(),
            co::COLOR::WINDOW,
            move |hwnd| {
                if errors.iter().any(|error| *error.hwnd() == *hwnd) {
                    RED
                } else if *muted.hwnd() == *hwnd {
                    GRAY
                } else {
                    BLACK
                }
            },
        );

        Self {
            page,
            frames,
            mode,
            backend,
            codec,
            frame_rate,
            buffer_seconds,
            bitrate,
            _spins: [frame_rate_spin, buffer_spin, bitrate_spin],
            hotkey,
            folder,
            estimate,
            apply,
            settings_error,
            start_with_windows: startup.check_box,
            startup: startup.group,
        }
    }

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    /// Finishes the parts that need the controls to exist.
    pub(super) fn on_create(&self) {
        layout::fit_titles(&self.frames);
        self.startup.on_create();
        self.update_estimate();
    }

    /// The extra height the page currently needs for its expanded group, in logical pixels.
    pub(super) fn extra(&self) -> i32 {
        self.startup.extra()
    }

    /// The configuration as currently entered, or the problem with it.
    pub(super) fn configuration(&self) -> Result<Config, String> {
        let required =
            |field: &gui::Edit| number(field).ok_or_else(|| text::NUMBER_REQUIRED.to_owned());
        let mode = chosen(&self.mode, &Mode::ALL);
        let backend = chosen(&self.backend, &Backend::ALL);
        let codec = chosen(&self.codec, &Codec::ALL);
        let hotkey = self.hotkey.text().unwrap_or_default();
        let folder = self.folder.text().unwrap_or_default();
        let folder = folder.trim();
        let default_folder = config::default_clip_directory().display().to_string();

        let mut config = Config {
            mode,
            backend,
            codec,
            frame_rate: required(&self.frame_rate)?,
            buffer_seconds: required(&self.buffer_seconds)?,
            bitrate_mbps: required(&self.bitrate)?,
            hotkey: hotkey.trim().to_owned(),
            clip_directory: if folder == default_folder {
                String::new()
            } else {
                folder.to_owned()
            },
        };
        config.validate().map_err(|error| error.to_string())?;
        // Stored in its canonical spelling, as the Status tab shows it.
        config.hotkey = config
            .parsed_hotkey()
            .map_err(|error| error.to_string())?
            .to_string();
        Ok(config)
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
                    / 1_000_000
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
    /// and reports whether the Startup group changed height.
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

    /// The Startup group stays compact until a Run-key error needs its extra line; reports
    /// whether its height changed, so the caller refits the window.
    pub(super) fn show_startup_error(&self, message: Option<&str>) -> bool {
        self.startup.set_message(message)
    }
}

/// The controls of the "Startup" group.
struct StartupControls {
    frame: Frame,
    check_box: gui::CheckBox,
    error: gui::Label,
    group: ExpandableGroup,
}

fn startup_controls(page: &gui::TabPage, start_with_windows: bool) -> StartupControls {
    // Compact; the group adds a line for the error label when one occurs.
    let startup_group = Frame::new(page, text::GROUP_STARTUP, 240, 78);
    let start_with_windows = gui::CheckBox::new(
        page,
        gui::CheckBoxOpts {
            text: text::START_WITH_WINDOWS,
            position: gui::dpi(INNER_X, 260),
            // Wide enough for the label in any font, rather than measured once at creation.
            size: gui::dpi(INNER_WIDTH, LINE),
            check_state: if start_with_windows {
                co::BST::CHECKED
            } else {
                co::BST::UNCHECKED
            },
            ..Default::default()
        },
    );
    let _startup_help = label(
        page,
        text::START_WITH_WINDOWS_HELP,
        INNER_X,
        280,
        INNER_WIDTH,
        2,
    );
    let startup_error = label(page, "", INNER_X, 280 + 2 * LINE, INNER_WIDTH, 1);
    let group = ExpandableGroup::new(
        startup_group.clone(),
        startup_error.clone(),
        STARTUP_ERROR_EXTRA,
    );
    StartupControls {
        frame: startup_group,
        check_box: start_with_windows,
        error: startup_error,
        group,
    }
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
        Mode::ALL.iter().position(|mode| *mode == config.mode),
    );
    let backend = choice_field(
        page,
        text::BACKEND,
        INNER_X,
        110,
        INNER_WIDTH - FIELD_OFFSET,
        &Backend::ALL.map(Backend::name),
        Backend::ALL
            .iter()
            .position(|backend| *backend == config.backend),
    );
    (mode, backend)
}

/// The whole number in `field`, if it holds one.
fn number(field: &gui::Edit) -> Option<u32> {
    field
        .text()
        .ok()
        .and_then(|value| value.trim().parse().ok())
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
    let _caption = label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1);
    let initial = value.to_string();
    let edit = gui::Edit::new(
        page,
        gui::EditOpts {
            text: &initial,
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(52),
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

/// A caption and a drop-down list of `items`, with `selected` chosen.
fn choice_field(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    width: i32,
    items: &[&str],
    selected: Option<usize>,
) -> gui::ComboBox {
    let _caption = label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1);
    gui::ComboBox::new(
        page,
        gui::ComboBoxOpts {
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(width),
            items,
            selected_item: selected.and_then(|index| u32::try_from(index).ok()),
            ..Default::default()
        },
    )
}

/// The choice a drop-down list shows, out of `all`, or the first when none is selected.
fn chosen<T: Copy + Default>(field: &gui::ComboBox, all: &[T]) -> T {
    field
        .items()
        .selected_index()
        .and_then(|index| all.get(index as usize).copied())
        .unwrap_or_default()
}

/// A caption and a free-text edit.
fn text_field(page: &gui::TabPage, caption: &str, y: i32, width: i32, value: &str) -> gui::Edit {
    let _caption = label(page, caption, INNER_X, y + 4, FIELD_OFFSET - 4, 1);
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
