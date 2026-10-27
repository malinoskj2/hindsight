//! The Settings tab.

use super::{
    hotkey_field::{self, Capture},
    layout::{
        self, BUTTON_HEIGHT, BUTTON_WIDTH, ExpandableGroup, Frame, INNER_WIDTH, INNER_X, LINE,
        Painted, TintedLabel, button, hidden_label, label, saturating_i32, shift_by, skin_button,
    },
    pane::{self, FIRST_FRAME, FIRST_ROW, Pane},
    text,
    theme::{Skin, Tone},
};
use crate::{
    audio::{self, DeviceList, Entry},
    config::{
        self, Backend, Codec, Config, ConfigError, Container, DeviceId, MAX_BITRATE_MBPS,
        MAX_BUFFER_SECONDS, MAX_FRAME_RATE, MIN_BITRATE_MBPS, MIN_BUFFER_SECONDS, MIN_FRAME_RATE,
        Memory, Mode, ScreenshotFormat, Theme,
    },
    log,
    replay::Source,
    sys::{
        hotkey::{self, RegisterError},
        shell, startup,
    },
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};
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
/// How much taller the window is with More options open: its rows push the page past the
/// height the other tabs need.
const MORE_OPTIONS_EXTRA: i32 =
    rows::GENERAL_FRAME + rows::GENERAL_HEIGHT - (layout::FILL_Y + layout::FILL_HEIGHT);

/// The rows as laid out with More options open. Closed, its rows are hidden and everything
/// below them moves up by [`rows::MORE_OPTIONS_HEIGHT`].
mod rows {
    use super::{BUTTON_HEIGHT, CHECK_PITCH, FIELD_PITCH, FIRST_FRAME, FIRST_ROW, LINE};

    pub(super) const RECORDING_FRAME: i32 = FIRST_FRAME;
    pub(super) const REPLAY_LENGTH: i32 = RECORDING_FRAME + FIRST_ROW;
    pub(super) const HOTKEY: i32 = REPLAY_LENGTH + FIELD_PITCH;
    pub(super) const MODE: i32 = HOTKEY + FIELD_PITCH;
    pub(super) const FOLDER: i32 = MODE + FIELD_PITCH;
    pub(super) const AUDIO: i32 = FOLDER + FIELD_PITCH;
    pub(super) const CURSOR: i32 = AUDIO + CHECK_PITCH;
    pub(super) const MORE_OPTIONS: i32 = CURSOR + CHECK_PITCH + 2;
    pub(super) const ENCODING: i32 = MORE_OPTIONS + BUTTON_HEIGHT + 8;
    pub(super) const BACKEND: i32 = ENCODING + FIELD_PITCH;
    pub(super) const MEMORY: i32 = BACKEND + FIELD_PITCH;
    pub(super) const FORMATS: i32 = MEMORY + FIELD_PITCH;
    pub(super) const SOUND_DEVICE: i32 = FORMATS + FIELD_PITCH;
    pub(super) const MICROPHONE_DEVICE: i32 = SOUND_DEVICE + FIELD_PITCH;
    pub(super) const ESTIMATE: i32 = MICROPHONE_DEVICE + FIELD_PITCH;
    pub(super) const APPLY: i32 = ESTIMATE + LINE + 4;
    pub(super) const MORE_OPTIONS_HEIGHT: i32 = APPLY - ENCODING;
    pub(super) const RECORDING_HEIGHT: i32 = APPLY + BUTTON_HEIGHT + 14 - RECORDING_FRAME;
    pub(super) const GENERAL_FRAME: i32 = RECORDING_FRAME + RECORDING_HEIGHT;
    pub(super) const THEME: i32 = GENERAL_FRAME + FIRST_ROW;
    pub(super) const START_WITH_WINDOWS: i32 = THEME + FIELD_PITCH;
    pub(super) const CLOSE_TO_TRAY: i32 = START_WITH_WINDOWS + CHECK_PITCH;
    pub(super) const STARTUP_ERROR: i32 = CLOSE_TO_TRAY + CHECK_PITCH;
    pub(super) const GENERAL_HEIGHT: i32 = CLOSE_TO_TRAY + LINE + 6 - GENERAL_FRAME;
}

/// Why the settings entered were not applied. Shown beside Apply.
#[derive(Debug, Error)]
pub(super) enum SettingsError {
    #[error("{}", text::NUMBER_REQUIRED)]
    NumberRequired,
    #[error(transparent)]
    Hotkey(#[from] hotkey::ParseError),
    #[error("screenshot hotkey: {0}")]
    ScreenshotHotkey(hotkey::ParseError),
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
    /// `None` when the devices could not be listed, so only the default is offered.
    devices: Option<DeviceList>,
    /// Whether More options is open. The controls are created open.
    more_options: Cell<bool>,
    /// Whether More options opens once the controls exist, as the settings left it.
    open_at_start: bool,
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
        self.update_device_choices();
        self.show_more_options(self.0.open_at_start);
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
        let devices = DeviceList::open()
            .inspect_err(|error| log::warning(text::devices_not_listed(error)))
            .ok();
        let recording = RecordingControls::new(&page, config, codecs, devices.as_ref(), skin);
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
            devices,
            more_options: Cell::new(true),
            open_at_start: config.more_options,
        });
        pane::paint(&inner.page, skin, Rc::clone(&inner));
        let page = Self(inner);
        page.estimate_events();
        page.browse_events();
        page.backend_events();
        page.sound_events();
        page
    }

    /// Lists the devices again if any came or went since they were listed, keeping each list's
    /// selection.
    pub(super) fn update_devices(&self) {
        let Some(devices) = self.0.devices.as_ref().filter(|devices| devices.changed()) else {
            return;
        };
        let choices = &self.0.recording.choices;
        for field in [&choices.sound_device, &choices.microphone_device] {
            field.refill(devices, field.chosen().as_ref());
        }
    }

    pub(super) fn extra(&self) -> i32 {
        let more_options = if self.0.more_options.get() {
            MORE_OPTIONS_EXTRA
        } else {
            0
        };
        self.0.general_group.extra() + more_options
    }

    /// `toggled` is called with whether More options is now open, once the page has moved.
    pub(super) fn on_more_options_toggled(&self, toggled: impl Fn(bool) + 'static) {
        let page = self.clone();
        self.0.recording.more_options.on().bn_clicked(move || {
            let open = !page.0.more_options.get();
            page.show_more_options(open);
            toggled(open);
            Ok(())
        });
    }

    /// Shows or hides More options' rows, moving everything below them.
    fn show_more_options(&self, open: bool) {
        if self.0.more_options.replace(open) == open {
            return;
        }
        let recording = &self.0.recording;
        for control in &recording.advanced {
            layout::show(&**control, open);
        }
        let delta = gui::dpi_y(if open {
            rows::MORE_OPTIONS_HEIGHT
        } else {
            -rows::MORE_OPTIONS_HEIGHT
        });
        let below = recording.below.iter().chain(&self.0.general.controls);
        for control in below {
            shift_by(control.hwnd(), delta);
        }
        if let [recording_frame, rest @ ..] = &self.0.frames[..] {
            recording_frame.grow(delta);
            for frame in rest {
                frame.shift(delta);
            }
        }
        layout::set_text(
            &recording.more_options,
            if open {
                text::FEWER_OPTIONS
            } else {
                text::MORE_OPTIONS
            },
        );
        let _ = self.0.page.hwnd().InvalidateRect(None, true);
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

    pub(super) fn on_hotkey_capture(&self, capture: impl Fn(Capture) + 'static) {
        let capture = Rc::new(capture);
        for field in self.hotkey_fields() {
            let began = Rc::clone(&capture);
            field.on().en_set_focus(move || {
                began(Capture::Began);
                Ok(())
            });
            let ended = Rc::clone(&capture);
            field.on().en_kill_focus(move || {
                ended(Capture::Ended);
                Ok(())
            });
        }
    }

    pub(super) fn on_close_to_tray_toggled(&self, toggled: impl Fn(bool) + 'static) {
        let close_to_tray = self.0.general.close_to_tray.clone();
        self.0.general.close_to_tray.on().bn_clicked(move || {
            toggled(close_to_tray.is_checked());
            Ok(())
        });
    }

    pub(super) fn on_update_checks_toggled(&self, toggled: impl Fn(bool) + 'static) {
        let check_for_updates = self.0.general.check_for_updates.clone();
        self.0.general.check_for_updates.on().bn_clicked(move || {
            toggled(check_for_updates.is_checked());
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

    pub(super) fn show_screenshot_hotkey(&self, hotkey: &str) {
        layout::set_text(&self.0.recording.screenshot_hotkey, hotkey);
    }

    /// Reports whether the General group changed height.
    pub(super) fn show_startup_error(&self, message: Option<&str>) -> bool {
        self.0.general_group.set_message(&self.0.frames, message)
    }

    fn hotkey_fields(&self) -> [&gui::Edit; 2] {
        [
            &self.0.recording.hotkey,
            &self.0.recording.screenshot_hotkey,
        ]
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
        self.0
            .recording
            .choices
            .memory
            .on()
            .cbn_sel_change(move || {
                page.update_estimate();
                Ok(())
            });
    }

    fn backend_events(&self) {
        let page = self.clone();
        self.0
            .recording
            .choices
            .backend
            .on()
            .cbn_sel_change(move || {
                page.update_cursor_choice();
                Ok(())
            });
    }

    fn sound_events(&self) {
        for check_box in [&self.0.recording.audio, &self.0.recording.microphone] {
            let page = self.clone();
            check_box.on().bn_clicked(move || {
                page.update_device_choices();
                Ok(())
            });
        }
    }

    /// A source's device list is disabled while the source is not recorded.
    fn update_device_choices(&self) {
        let recording = &self.0.recording;
        let choices = &recording.choices;
        for (field, check_box) in [
            (&choices.sound_device, &recording.audio),
            (&choices.microphone_device, &recording.microphone),
        ] {
            field.field.hwnd().EnableWindow(check_box.is_checked());
        }
    }

    /// DXGI Desktop Duplication cannot draw the cursor, so the cursor choice is disabled while
    /// that backend is selected.
    fn update_cursor_choice(&self) {
        let recording = &self.0.recording;
        let backend = chosen(&recording.choices.backend, &recording.choices.backends);
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
        let memory = chosen(&recording.choices.memory, &Memory::ALL);
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
        let screenshot_hotkey = recording.screenshot_hotkey.text().unwrap_or_default();
        let folder = recording.folder.text().unwrap_or_default();
        Ok(Config {
            mode: chosen(&recording.mode, &Mode::ALL),
            backend: chosen(&recording.choices.backend, &recording.choices.backends),
            codec: chosen(&encoding.codec, &self.0.codecs),
            frame_rate: required(&encoding.frame_rate)?,
            buffer_seconds: required(&encoding.buffer_seconds)?,
            bitrate_mbps: required(&encoding.bitrate)?,
            hotkey: hotkey.trim().parse()?,
            screenshot_hotkey: screenshot_hotkey
                .trim()
                .parse()
                .map_err(SettingsError::ScreenshotHotkey)?,
            clip_directory: stored_folder(folder.trim()),
            audio: recording.audio.is_checked(),
            microphone: recording.microphone.is_checked(),
            sound_device: recording.choices.sound_device.chosen(),
            microphone_device: recording.choices.microphone_device.chosen(),
            cursor: recording.cursor.is_checked(),
            buffer_memory: chosen(&recording.choices.memory, &Memory::ALL),
            theme: chosen(&self.0.general.theme, &Theme::ALL),
            close_to_tray: self.0.general.close_to_tray.is_checked(),
            check_for_updates: self.0.general.check_for_updates.is_checked(),
            container: chosen(&recording.choices.container, &Container::ALL),
            screenshot_format: chosen(&recording.choices.screenshot_format, &ScreenshotFormat::ALL),
            more_options: self.0.more_options.get(),
        })
    }
}

/// A control that More options shows or that moves when it opens.
type Control = Box<dyn GuiWindow>;

struct RecordingControls {
    encoding: EncodingFields,
    hotkey: gui::Edit,
    screenshot_hotkey: gui::Edit,
    mode: gui::ComboBox,
    choices: AdvancedChoices,
    folder: gui::Edit,
    browse: gui::Button,
    audio: gui::CheckBox,
    microphone: gui::CheckBox,
    cursor: gui::CheckBox,
    more_options: gui::Button,
    estimate: TintedLabel,
    apply: gui::Button,
    error: TintedLabel,
    /// More options' rows, captions included.
    advanced: Vec<Control>,
    /// The controls under More options' rows.
    below: Vec<Control>,
}

impl RecordingControls {
    fn new(
        page: &gui::TabPage,
        config: &Config,
        codecs: &[Codec],
        devices: Option<&DeviceList>,
        skin: &Skin,
    ) -> Self {
        let (encoding, mut advanced) = EncodingFields::new(page, config, codecs);
        let [hotkey, screenshot_hotkey] = hotkey_fields(page, config);
        let (mode, _) = choice_field(
            page,
            text::MODE,
            INNER_X,
            rows::MODE,
            CHOICE_WIDTH,
            &Mode::ALL.map(Mode::name),
            index_of(&Mode::ALL, config.mode),
        );
        let choices = AdvancedChoices::new(page, config, devices);
        let folder = text_field(
            page,
            text::FOLDER,
            rows::FOLDER,
            FOLDER_WIDTH,
            &config.clip_directory().display().to_string(),
        );
        let browse = button(page, text::BROWSE, BROWSE_X, rows::FOLDER, BROWSE_WIDTH);
        skin_button(&browse, skin);
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
        let more_options = button(
            page,
            text::FEWER_OPTIONS,
            INNER_X,
            rows::MORE_OPTIONS,
            BUTTON_WIDTH,
        );
        skin_button(&more_options, skin);
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
        advanced.extend(choices.controls());
        advanced.push(Box::new(estimate.label().clone()));
        let below: Vec<Control> = vec![Box::new(apply.clone()), Box::new(error.label().clone())];
        Self {
            encoding,
            hotkey,
            screenshot_hotkey,
            mode,
            choices,
            folder,
            browse,
            audio,
            microphone,
            cursor,
            more_options,
            estimate,
            apply,
            error,
            advanced,
            below,
        }
    }
}

/// The save and screenshot hotkeys, each taking a pressed combination.
fn hotkey_fields(page: &gui::TabPage, config: &Config) -> [gui::Edit; 2] {
    let hotkey = text_field(
        page,
        text::HOTKEY,
        rows::HOTKEY,
        HOTKEY_WIDTH,
        &config.hotkey.to_string(),
    );
    let screenshot_hotkey = text_field_at(
        page,
        text::SCREENSHOT_HOTKEY,
        COLUMN_X,
        rows::HOTKEY,
        HOTKEY_WIDTH,
        &config.screenshot_hotkey.to_string(),
    );
    for field in [&hotkey, &screenshot_hotkey] {
        hotkey_field::capture(field);
    }
    [hotkey, screenshot_hotkey]
}

/// The lists under More options besides the codec.
struct AdvancedChoices {
    backend: gui::ComboBox,
    /// The backends offered, in the order of the backend list's items.
    backends: Vec<Backend>,
    memory: gui::ComboBox,
    container: gui::ComboBox,
    screenshot_format: gui::ComboBox,
    sound_device: DeviceField,
    microphone_device: DeviceField,
    captions: [gui::Label; 4],
}

impl AdvancedChoices {
    fn new(page: &gui::TabPage, config: &Config, devices: Option<&DeviceList>) -> Self {
        let backends = Backend::offered(config.backend);
        let (backend, backend_caption) = choice_field(
            page,
            text::BACKEND,
            INNER_X,
            rows::BACKEND,
            FULL_WIDTH,
            &backends
                .iter()
                .map(|backend| backend.name())
                .collect::<Vec<_>>(),
            index_of(&backends, config.backend),
        );
        let (memory, memory_caption) = choice_field(
            page,
            text::MEMORY,
            INNER_X,
            rows::MEMORY,
            CHOICE_WIDTH,
            &Memory::ALL.map(Memory::name),
            index_of(&Memory::ALL, config.buffer_memory),
        );
        let (container, container_caption) = choice_field(
            page,
            text::CONTAINER,
            INNER_X,
            rows::FORMATS,
            CODEC_WIDTH,
            &Container::ALL.map(Container::name),
            index_of(&Container::ALL, config.container),
        );
        let (screenshot_format, screenshot_format_caption) = choice_field(
            page,
            text::SCREENSHOT_FORMAT,
            COLUMN_X,
            rows::FORMATS,
            CODEC_WIDTH,
            &ScreenshotFormat::ALL.map(ScreenshotFormat::name),
            index_of(&ScreenshotFormat::ALL, config.screenshot_format),
        );
        let sound_device = DeviceField::new(
            page,
            text::SOUND_DEVICE,
            rows::SOUND_DEVICE,
            Source::System,
            devices,
            config.sound_device.as_ref(),
        );
        let microphone_device = DeviceField::new(
            page,
            text::MICROPHONE_DEVICE,
            rows::MICROPHONE_DEVICE,
            Source::Microphone,
            devices,
            config.microphone_device.as_ref(),
        );
        Self {
            backend,
            backends,
            memory,
            container,
            screenshot_format,
            sound_device,
            microphone_device,
            captions: [
                backend_caption,
                memory_caption,
                container_caption,
                screenshot_format_caption,
            ],
        }
    }

    fn controls(&self) -> impl Iterator<Item = Control> {
        let devices = [&self.sound_device, &self.microphone_device];
        [
            &self.backend,
            &self.memory,
            &self.container,
            &self.screenshot_format,
        ]
        .into_iter()
        .chain(devices.map(|device| &device.field))
        .map(|field| Box::new(field.clone()) as Control)
        .chain(
            self.captions
                .iter()
                .chain(devices.map(|device| &device.caption))
                .map(|caption| Box::new(caption.clone()) as Control),
        )
    }
}

/// A list of the devices a source can be captured from, Windows' default first.
struct DeviceField {
    source: Source,
    field: gui::ComboBox,
    caption: gui::Label,
    /// The list's entries, in the order of its items.
    entries: RefCell<Vec<Entry>>,
}

impl DeviceField {
    fn new(
        page: &gui::TabPage,
        caption: &str,
        y: i32,
        source: Source,
        devices: Option<&DeviceList>,
        chosen: Option<&DeviceId>,
    ) -> Self {
        let (entries, selected) = device_entries(source, devices, chosen);
        let labels: Vec<String> = entries.iter().map(entry_label).collect();
        let (field, caption) = choice_field(
            page,
            caption,
            INNER_X,
            y,
            FULL_WIDTH,
            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            u32::try_from(selected).ok(),
        );
        Self {
            source,
            field,
            caption,
            entries: RefCell::new(entries),
        }
    }

    /// Lists the devices there are now, selecting `selected`.
    fn refill(&self, devices: &DeviceList, selected: Option<&DeviceId>) {
        let (entries, position) = device_entries(self.source, Some(devices), selected);
        let labels: Vec<String> = entries.iter().map(entry_label).collect();
        let items = self.field.items();
        items.delete_all();
        let _ = items.add(&labels);
        items.select(u32::try_from(position).ok());
        *self.entries.borrow_mut() = entries;
    }

    /// Returns the device selected, or `None` for the default.
    fn chosen(&self) -> Option<DeviceId> {
        let index = usize::try_from(self.field.items().selected_index()?).ok()?;
        self.entries.borrow().get(index)?.device().cloned()
    }
}

/// Returns the entries of `source`'s device list and the position of `selected` among them.
fn device_entries(
    source: Source,
    devices: Option<&DeviceList>,
    selected: Option<&DeviceId>,
) -> (Vec<Entry>, usize) {
    let present = devices.map_or_else(|| Ok(Vec::new()), |devices| devices.endpoints(source));
    let present = present.unwrap_or_else(|error| {
        log::warning(text::devices_not_listed(&error));
        Vec::new()
    });
    audio::entries(present, selected, |id| {
        devices.and_then(|devices| devices.name(id))
    })
}

fn entry_label(entry: &Entry) -> String {
    match entry {
        Entry::Default => text::DEFAULT_DEVICE.to_owned(),
        Entry::Present(endpoint) => endpoint.name.clone(),
        Entry::Absent { name, .. } => text::absent_device(name.as_deref()),
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
    /// Also returns the fields under More options: bitrate and codec.
    fn new(page: &gui::TabPage, config: &Config, codecs: &[Codec]) -> (Self, Vec<Control>) {
        let (buffer_seconds, buffer_spin, _) = number_field(
            page,
            text::REPLAY_LENGTH,
            INNER_X,
            rows::REPLAY_LENGTH,
            (MIN_BUFFER_SECONDS, MAX_BUFFER_SECONDS),
            config.buffer_seconds,
        );
        let (frame_rate, frame_rate_spin, _) = number_field(
            page,
            text::FRAME_RATE,
            COLUMN_X,
            rows::REPLAY_LENGTH,
            (MIN_FRAME_RATE, MAX_FRAME_RATE),
            config.frame_rate,
        );
        let (bitrate, bitrate_spin, bitrate_caption) = number_field(
            page,
            text::BITRATE,
            INNER_X,
            rows::ENCODING,
            (MIN_BITRATE_MBPS, MAX_BITRATE_MBPS),
            config.bitrate_mbps,
        );
        let names = codecs.iter().map(|codec| codec.name()).collect::<Vec<_>>();
        let (codec, codec_caption) = choice_field(
            page,
            text::CODEC,
            COLUMN_X,
            rows::ENCODING,
            CODEC_WIDTH,
            &names,
            index_of(codecs, config.codec).or(Some(0)),
        );
        let advanced: Vec<Control> = vec![
            Box::new(bitrate.clone()),
            Box::new(bitrate_spin.clone()),
            Box::new(bitrate_caption),
            Box::new(codec.clone()),
            Box::new(codec_caption),
        ];
        let fields = Self {
            buffer_seconds,
            bitrate,
            codec,
            frame_rate,
            _spins: [buffer_spin, bitrate_spin, frame_rate_spin],
        };
        (fields, advanced)
    }
}

struct GeneralControls {
    theme: gui::ComboBox,
    start_with_windows: gui::CheckBox,
    close_to_tray: gui::CheckBox,
    check_for_updates: gui::CheckBox,
    error: TintedLabel,
    /// Every control in the group, which moves with it when More options opens or closes.
    controls: Vec<Control>,
}

impl GeneralControls {
    fn new(page: &gui::TabPage, config: &Config, start_with_windows: bool) -> Self {
        let (theme, theme_caption) = choice_field(
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
        let close_to_tray = check_box(
            page,
            text::CLOSE_TO_TRAY,
            INNER_X,
            rows::CLOSE_TO_TRAY,
            COLUMN_X - INNER_X,
            config.close_to_tray,
        );
        let check_for_updates = check_box(
            page,
            text::CHECK_FOR_UPDATES,
            COLUMN_X,
            rows::CLOSE_TO_TRAY,
            CHECK_COLUMN_WIDTH,
            config.check_for_updates,
        );
        let error = TintedLabel::new(
            hidden_label(page, INNER_X, rows::STARTUP_ERROR, INNER_WIDTH, 1),
            Tone::Error,
        );
        let controls: Vec<Control> = vec![
            Box::new(theme.clone()),
            Box::new(theme_caption),
            Box::new(start_with_windows.clone()),
            Box::new(close_to_tray.clone()),
            Box::new(check_for_updates.clone()),
            Box::new(error.label().clone()),
        ];
        Self {
            theme,
            start_with_windows,
            close_to_tray,
            check_for_updates,
            error,
            controls,
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
) -> (gui::ComboBox, gui::Label) {
    let caption = field_caption(page, caption, x, y);
    let field = gui::ComboBox::new(
        page,
        gui::ComboBoxOpts {
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(width),
            items,
            selected_item: selected,
            ..Default::default()
        },
    );
    (field, caption)
}

fn number_field(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    (minimum, maximum): (u32, u32),
    value: u32,
) -> (gui::Edit, gui::UpDown, gui::Label) {
    let caption = field_caption(page, caption, x, y);
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
    (edit, spin, caption)
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
    text_field_at(page, caption, INNER_X, y, width, value)
}

fn text_field_at(
    page: &gui::TabPage,
    caption: &str,
    x: i32,
    y: i32,
    width: i32,
    value: &str,
) -> gui::Edit {
    let _caption = field_caption(page, caption, x, y);
    gui::Edit::new(
        page,
        gui::EditOpts {
            text: value,
            position: gui::dpi(x + FIELD_OFFSET, y),
            width: gui::dpi_x(width),
            control_style: co::ES::AUTOHSCROLL | co::ES::NOHIDESEL,
            ..Default::default()
        },
    )
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

fn field_caption(page: &gui::TabPage, caption: &str, x: i32, y: i32) -> gui::Label {
    label(page, caption, x, y + 4, FIELD_OFFSET - 4, 1)
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
    fn device_lists_name_the_default_and_a_chosen_device_that_is_not_connected() {
        let id = DeviceId::new("{0.0.1.00000000}.{headset}".to_owned()).unwrap();
        assert_eq!(entry_label(&Entry::Default), text::DEFAULT_DEVICE);
        assert_eq!(
            entry_label(&Entry::Absent {
                id: id.clone(),
                name: Some("Headset".to_owned())
            }),
            "Headset (not connected)"
        );
        assert_eq!(
            entry_label(&Entry::Absent { id, name: None }),
            "A device not connected"
        );
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
        let parse = "".parse::<hotkey::Hotkey>().unwrap_err();
        assert_eq!(
            SettingsError::ScreenshotHotkey(parse).to_string(),
            format!("screenshot hotkey: {shown}")
        );
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
                rows::HOTKEY,
                rows::MODE,
                rows::FOLDER,
                rows::AUDIO,
                rows::CURSOR,
                rows::MORE_OPTIONS,
            ],
            [26, 54, 82, 110, 138, 160, 184]
        );
        assert_eq!(
            [
                rows::ENCODING,
                rows::BACKEND,
                rows::MEMORY,
                rows::FORMATS,
                rows::SOUND_DEVICE,
                rows::MICROPHONE_DEVICE,
                rows::ESTIMATE,
                rows::APPLY,
                rows::MORE_OPTIONS_HEIGHT,
            ],
            [216, 244, 272, 300, 328, 356, 384, 406, 190]
        );
        assert_eq!((rows::RECORDING_FRAME, rows::RECORDING_HEIGHT), (6, 438));
        assert_eq!((rows::GENERAL_FRAME, rows::GENERAL_HEIGHT), (444, 94));
        assert_eq!(
            [
                rows::THEME,
                rows::START_WITH_WINDOWS,
                rows::CLOSE_TO_TRAY,
                rows::STARTUP_ERROR
            ],
            [464, 492, 514, 536]
        );
    }

    #[test]
    fn closed_the_page_fits_the_window_and_open_it_grows_the_window_to_fit() {
        let bottom = rows::GENERAL_FRAME + rows::GENERAL_HEIGHT;
        let fill = layout::FILL_Y + layout::FILL_HEIGHT;
        assert!(bottom - rows::MORE_OPTIONS_HEIGHT <= fill);
        assert_eq!(MORE_OPTIONS_EXTRA, 118);
        assert_eq!(bottom, fill + MORE_OPTIONS_EXTRA);
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
