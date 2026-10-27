//! The main window: a plain Win32 dialog built from the system's common controls.

use self::{
    about::{AboutPage, UpdateAction},
    hotkey_field::Capture,
    layout::{MARGIN, TAB_HEIGHT, WINDOW_HEIGHT, WINDOW_WIDTH, resize_height_by, resize_width_by},
    logs::LogsPage,
    media::{Media, MediaPage},
    pane::Pane,
    settings::{SettingsError, SettingsPage},
    status::{Control, StatusPage},
    theme::{Skin, Surface},
};
use crate::{
    APP_NAME,
    config::{Codec, Config, Mode, Preference, Theme},
    gpu, log,
    service::{Target, Worker},
    sys::{
        foreground::{self, ForegroundHook},
        hotkey::{self, Action, RegisterError},
        icon, instance,
        shell::{self, Elevation},
        startup,
        tray::{Tray, TrayAction},
        window,
    },
    update::{
        Check, Installation, Release, State, Version,
        client::{CheckError, Done, DownloadError, Installer, Updater},
        notes::Arrival,
    },
};
use anyhow::Result;
use std::{
    cell::{Cell, OnceCell, RefCell},
    fs, iter,
    path::Path,
    rc::Rc,
};
use winsafe::{self as w, co, gui, prelude::*};

mod about;
mod art;
mod hotkey_field;
mod layout;
mod logs;
mod media;
mod pane;
mod settings;
mod startup_error;
mod status;
mod text;
mod theme;
mod whats_new;

const ICON_RESOURCE: u16 = icon::resource(Theme::Classic);
const STATUS_TIMER: usize = 1;
const STATUS_INTERVAL_MS: u32 = 1000;
const CLIPS_TAB: u32 = 1;
const SCREENSHOTS_TAB: u32 = 2;
const PANES: usize = 6;
const DESTROY_TIMER: usize = 2;
const SETTLE_TIMER: usize = 3;
const SETTLE_MS: u32 = 500;
const UPDATE_TIMER: usize = 4;
/// The first check waits, since at sign-in the network may not be up yet.
const FIRST_UPDATE_CHECK_MS: u32 = 60 * 1000;
const UPDATE_INTERVAL_MS: u32 = 24 * 60 * 60 * 1000;
/// Runs while the update thread has a job.
const UPDATE_POLL_TIMER: usize = 5;
const UPDATE_POLL_MS: u32 = 250;
/// Shows the notes after an upgrade, once the window that was just shown has painted.
const WHATS_NEW_TIMER: usize = 6;

pub(crate) fn run(startup: Result<(Worker, Config, Arrival)>, start_hidden: bool) -> Result<()> {
    match startup {
        Ok((worker, config, arrival)) => {
            Main::create_and_run(worker, config, arrival, start_hidden)
        }
        Err(error) => startup_error::show(&format!("{error:#}"), ICON_RESOURCE),
    }
}

#[derive(Clone)]
struct Main(Rc<Inner>);

/// The cells are borrowed briefly, never across a call that can re-enter the window procedure.
struct Inner {
    main_window: gui::WindowMain,
    tab: gui::Tab,
    status: StatusPage,
    clips: MediaPage,
    screenshots: MediaPage,
    settings: SettingsPage,
    logs: LogsPage,
    about: AboutPage,
    skin: Skin,
    worker: Worker,
    config: RefCell<Config>,
    tray: RefCell<Option<Tray>>,
    foreground_hook: RefCell<Option<ForegroundHook>>,
    /// The full-screen app seen most recently, offered to the worker once it has stayed in front.
    pending_target: RefCell<Option<Target>>,
    quitting: Cell<bool>,
    /// Indexed by [`Action::index`].
    hotkey_errors: RefCell<[Option<String>; 2]>,
    tray_error: OnceCell<String>,
    initial_startup_error: Cell<Option<String>>,
    /// The height the pages have grown by to show their messages.
    extra: Cell<i32>,
    /// `None` when its thread could not start.
    updater: Option<Updater>,
    update_state: RefCell<State>,
    installation: Installation,
    /// `Upgraded` until the notes have been shown, which waits for the window to be.
    arrival: Cell<Arrival>,
}

impl Main {
    fn create_and_run(
        worker: Worker,
        config: Config,
        arrival: Arrival,
        start_hidden: bool,
    ) -> Result<()> {
        let main = Self::create(worker, config, arrival, start_hidden);
        main.draw();
        main.window_events();
        main.status_events();
        main.settings_events();
        main.update_events();
        main.run(start_hidden)
    }

    fn create(worker: Worker, config: Config, arrival: Arrival, start_hidden: bool) -> Self {
        let skin = Skin::new(config.theme);
        let width = WINDOW_WIDTH + skin.extra_width();
        let main_window = application_window(width, start_hidden);
        let (start_with_windows, startup_error) = startup_setting();
        let status = StatusPage::new(&main_window, &skin);
        let clips = MediaPage::new(&main_window, Media::Clips, &skin);
        let screenshots = MediaPage::new(&main_window, Media::Screenshots, &skin);
        let codecs = offered_codecs();
        let settings = SettingsPage::new(&main_window, &config, &codecs, start_with_windows, &skin);
        let logs = LogsPage::new(&main_window, &skin);
        let about = AboutPage::new(&main_window, &skin);
        let panes: [&dyn Pane; PANES] = [&status, &clips, &screenshots, &settings, &logs, &about];
        let tab_pages = panes.map(|pane| (pane.title(), pane.page().clone()));
        let tab = gui::Tab::new(
            &main_window,
            gui::TabOpts {
                position: gui::dpi(MARGIN, MARGIN),
                size: gui::dpi(width - 2 * MARGIN, TAB_HEIGHT),
                pages: &tab_pages,
                ..Default::default()
            },
        );
        Self(Rc::new(Inner {
            main_window,
            tab,
            status,
            clips,
            screenshots,
            settings,
            logs,
            about,
            skin,
            worker,
            config: RefCell::new(config),
            tray: RefCell::default(),
            foreground_hook: RefCell::default(),
            pending_target: RefCell::default(),
            quitting: Cell::new(false),
            hotkey_errors: RefCell::default(),
            tray_error: OnceCell::new(),
            initial_startup_error: Cell::new(startup_error),
            extra: Cell::new(0),
            updater: Updater::start()
                .inspect_err(|error| log::warning(text::updates_unavailable(error)))
                .ok(),
            update_state: RefCell::new(State::Unchecked),
            installation: std::env::current_exe().map_or(Installation::Portable, |executable| {
                Installation::of(&executable)
            }),
            arrival: Cell::new(arrival),
        }))
    }

    fn draw(&self) {
        let skin = self.0.skin.clone();
        let tab = self.0.tab.clone();
        self.0.tab.on_subclass().wm_paint(move || {
            if skin.is_themed() {
                skin.paint_tabs(&tab);
            } else {
                theme::paint_tabs_natively(tab.hwnd());
            }
            Ok(())
        });
        layout::draw(
            &self.0.main_window,
            self.0.main_window.on(),
            &self.0.skin,
            Surface::Dialog,
            (),
        );
    }

    fn window_events(&self) {
        let this = self.clone();
        self.0.main_window.on().wm_create(move |_| {
            this.on_create();
            Ok(0)
        });

        let this = self.clone();
        self.0.tab.on().tcn_sel_change(move || {
            this.refresh();
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm_show_window(move |params| {
            this.on_show_window(params.being_shown);
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm_timer(STATUS_TIMER, move || {
            this.on_timer();
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm_timer(SETTLE_TIMER, move || {
            this.settle_foreground();
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm_timer(UPDATE_TIMER, move || {
            this.on_update_timer();
            Ok(())
        });

        let this = self.clone();
        self.0
            .main_window
            .on()
            .wm_timer(UPDATE_POLL_TIMER, move || {
                this.poll_updates();
                Ok(())
            });

        let this = self.clone();
        self.0.main_window.on().wm_timer(WHATS_NEW_TIMER, move || {
            this.on_whats_new_timer();
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm(co::WM::HOTKEY, move |message| {
            this.on_hotkey(message.wparam);
            Ok(0)
        });

        let this = self.clone();
        self.0
            .main_window
            .on()
            .wm(instance::SHOW_MESSAGE, move |_| {
                this.show();
                Ok(0)
            });

        let this = self.clone();
        self.0.main_window.on().wm_close(move || {
            this.on_close();
            Ok(())
        });

        let this = self.clone();
        self.0.main_window.on().wm_destroy(move || {
            this.on_destroy();
            Ok(())
        });
    }

    fn status_events(&self) {
        let this = self.clone();
        self.0
            .status
            .on_control(move |control| this.on_control(control));
    }

    fn on_control(&self, control: Control) {
        match control {
            Control::Save => self.save(),
            Control::PauseOrResume => {
                self.0.worker.toggle_pause();
                // Offered after the toggle, so only a resumed worker accepts it.
                self.watch_foreground();
            }
            Control::Restart => {
                self.0.worker.restart();
                self.watch_foreground();
            }
            Control::OpenClip => {
                if let Some(clip) = self.0.worker.status().last_clip {
                    open(&clip);
                }
            }
            Control::OpenFolder => {
                let folder = self.0.config.borrow().clip_directory();
                if let Err(error) = fs::create_dir_all(&folder) {
                    log::warning(text::open_failed(&folder, error));
                    return;
                }
                open(&folder);
            }
        }
    }

    fn settings_events(&self) {
        let this = self.clone();
        self.0.settings.on_apply(move || this.apply_settings());

        let this = self.clone();
        self.0
            .settings
            .on_theme_chosen(move |theme| this.on_theme_chosen(theme));

        let this = self.clone();
        self.0
            .settings
            .on_startup_toggled(move || this.fit_window());

        let this = self.clone();
        self.0
            .settings
            .on_close_to_tray_toggled(move |close_to_tray| {
                this.on_close_to_tray_toggled(close_to_tray);
            });

        let this = self.clone();
        self.0
            .settings
            .on_hotkey_capture(move |capture| this.on_hotkey_capture(capture));

        let this = self.clone();
        self.0
            .settings
            .on_update_checks_toggled(move |check| this.on_update_checks_toggled(check));

        let this = self.clone();
        self.0
            .settings
            .on_advanced_toggled(move |open| this.on_advanced_toggled(open));
    }

    fn update_events(&self) {
        let this = self.clone();
        self.0
            .about
            .on_update_action(move |action| this.on_update_action(action));
    }

    fn run(&self, start_hidden: bool) -> Result<()> {
        let show = if start_hidden {
            co::SW::HIDE
        } else {
            co::SW::SHOW
        };
        layout::run(
            &self.0.main_window,
            Some(show),
            "run the application window",
        )
    }

    fn on_create(&self) {
        self.apply_look();
        for pane in self.panes() {
            pane.on_create();
        }
        self.fit_window();
        self.show_initial_startup_error();
        self.register_configured_hotkeys();
        self.watch_foreground_changes();
        self.add_tray_icon();
        self.start_update_checks();
        self.refresh();
    }

    fn show_initial_startup_error(&self) {
        if let Some(error) = self.0.initial_startup_error.take()
            && self.0.settings.show_startup_error(Some(&error))
        {
            self.fit_window();
        }
    }

    fn watch_foreground_changes(&self) {
        let this = self.clone();
        let hook = ForegroundHook::install(move || this.watch_foreground());
        if hook.is_none() {
            log::warning(text::FOREGROUND_WATCH_FAILED);
        }
        *self.0.foreground_hook.borrow_mut() = hook;
        self.watch_foreground();
    }

    fn add_tray_icon(&self) {
        let this = self.clone();
        match Tray::new(self.0.skin.theme(), move |action| this.on_tray(action)) {
            Ok(tray) => *self.0.tray.borrow_mut() = Some(tray),
            Err(error) => {
                let message = text::tray_unavailable(log::chain(&error));
                log::warning(&message);
                let _ = self.0.tray_error.set(message);
            }
        }
    }

    /// The refresh timer runs only while the window is visible.
    fn on_show_window(&self, being_shown: bool) {
        if being_shown {
            let _ = self
                .0
                .main_window
                .hwnd()
                .SetTimer(STATUS_TIMER, STATUS_INTERVAL_MS, None);
            if self.0.arrival.get() == Arrival::Upgraded {
                let _ = self.0.main_window.hwnd().SetTimer(WHATS_NEW_TIMER, 0, None);
            }
            self.refresh();
        } else {
            let _ = self.0.main_window.hwnd().KillTimer(STATUS_TIMER);
        }
    }

    /// Shows the notes after an upgrade, then records the version so they are not shown again.
    fn on_whats_new_timer(&self) {
        let _ = self.0.main_window.hwnd().KillTimer(WHATS_NEW_TIMER);
        if !self.0.main_window.hwnd().IsWindowVisible()
            || self.0.arrival.replace(Arrival::Again) != Arrival::Upgraded
        {
            return;
        }
        self.show_whats_new();
        self.0.worker.record_version(Version::running());
    }

    fn show_whats_new(&self) {
        whats_new::show(&self.0.main_window, &self.0.skin);
    }

    fn on_timer(&self) {
        self.0.worker.refresh();
        self.refresh();
    }

    fn on_hotkey(&self, id: usize) {
        match Action::from_id(id) {
            Some(Action::Save) => self.save(),
            Some(Action::Screenshot) => self.0.worker.screenshot(foreground::in_front()),
            None => {}
        }
    }

    fn show(&self) {
        self.0.main_window.hwnd().ShowWindow(co::SW::RESTORE);
        self.0.main_window.hwnd().SetForegroundWindow();
    }

    fn on_close(&self) {
        let to_tray = self.0.tray.borrow().is_some()
            && !self.0.quitting.get()
            && self.0.config.borrow().close_to_tray;
        if to_tray {
            log::info(text::CLOSED_TO_TRAY);
            self.0.main_window.hwnd().ShowWindow(co::SW::HIDE);
            return;
        }
        if !self.0.quitting.get() {
            log::info(text::QUIT_ON_CLOSE);
        }
        // winsafe frees its handlers, this one included, when the window is destroyed. The
        // timer callback is dispatched straight from the message loop, so no handler is running.
        let _ = self
            .0
            .main_window
            .hwnd()
            .SetTimer(DESTROY_TIMER, 0, Some(destroy_window));
    }

    fn on_destroy(&self) {
        // Dropped after the borrows end, since the tray's teardown can re-enter and borrow again.
        let tray = self.0.tray.take();
        let hook = self.0.foreground_hook.take();
        drop(tray);
        drop(hook);
    }

    fn on_tray(&self, action: TrayAction) {
        match action {
            TrayAction::Show => self.show(),
            TrayAction::Save => self.save(),
            TrayAction::Quit => {
                log::info(text::QUIT_FROM_TRAY);
                self.0.quitting.set(true);
                // Closed from the message loop, not from inside the tray's own callback.
                self.0.main_window.close();
            }
        }
    }

    /// Records the full-screen app in front, if any, and offers it to the worker once it has
    /// stayed there for `SETTLE_MS`.
    fn watch_foreground(&self) {
        if self.0.config.borrow().mode != Mode::Fullscreen {
            return;
        }
        let Some(target) = foreground::fullscreen_app() else {
            return;
        };
        if self.0.pending_target.borrow().as_ref() == Some(&target) {
            return;
        }
        *self.0.pending_target.borrow_mut() = Some(target);
        let _ = self
            .0
            .main_window
            .hwnd()
            .SetTimer(SETTLE_TIMER, SETTLE_MS, None);
    }

    /// Handles the settle timer. The recorded app is offered only if it is still the one in
    /// front.
    fn settle_foreground(&self) {
        let _ = self.0.main_window.hwnd().KillTimer(SETTLE_TIMER);
        let Some(pending) = self.0.pending_target.borrow_mut().take() else {
            return;
        };
        if foreground::fullscreen_app().as_ref() == Some(&pending) {
            self.0.worker.target(pending);
        }
    }

    fn save(&self) {
        self.0.worker.save();
        if self.0.main_window.hwnd().IsWindowVisible() {
            self.refresh();
        }
    }

    fn apply_settings(&self) {
        let config = match self.0.settings.configuration() {
            Ok(entered) => entered,
            Err(error) => {
                self.0.settings.show_settings_error(Some(&error));
                return;
            }
        };
        if let Err(error) = self.switch_hotkeys(&config) {
            let error = SettingsError::from(error);
            self.0.settings.show_settings_error(Some(&error));
            return;
        }
        self.0.settings.show_settings_error(None);
        self.0.settings.show_hotkey(&config.hotkey.to_string());
        self.0
            .settings
            .show_screenshot_hotkey(&config.screenshot_hotkey.to_string());
        *self.0.config.borrow_mut() = config.clone();
        self.0.worker.start(config);
        self.watch_foreground();
    }

    fn on_theme_chosen(&self, theme: Theme) {
        if theme == self.0.skin.theme() {
            return;
        }
        let before = self.0.skin.extra_width();
        self.0.skin.set(theme);
        let delta = gui::dpi_x(self.0.skin.extra_width() - before);
        if delta != 0 {
            self.widen(delta);
        }
        self.remember(Preference::Theme(theme));
        self.apply_look();
    }

    fn on_close_to_tray_toggled(&self, close_to_tray: bool) {
        self.remember(Preference::CloseToTray(close_to_tray));
    }

    fn on_advanced_toggled(&self, open: bool) {
        self.remember(Preference::MoreOptions(open));
        self.fit_window();
    }

    fn on_update_checks_toggled(&self, check: bool) {
        self.remember(Preference::CheckForUpdates(check));
        if check && matches!(*self.0.update_state.borrow(), State::Unchecked) {
            self.check_for_updates();
        }
    }

    /// Applies a setting that leaves recording alone to the window's copy of the settings and
    /// has the worker save it.
    fn remember(&self, preference: Preference) {
        preference.apply_to(&mut self.0.config.borrow_mut());
        self.0.worker.remember(preference);
    }

    /// Checks a minute after starting, if checks are on, and then once a day while hindsight
    /// runs.
    fn start_update_checks(&self) {
        let _ = self
            .0
            .main_window
            .hwnd()
            .SetTimer(UPDATE_TIMER, FIRST_UPDATE_CHECK_MS, None);
        self.show_update();
    }

    fn on_update_timer(&self) {
        let _ = self
            .0
            .main_window
            .hwnd()
            .SetTimer(UPDATE_TIMER, UPDATE_INTERVAL_MS, None);
        if self.0.config.borrow().check_for_updates {
            self.check_for_updates();
        }
    }

    fn on_update_action(&self, action: UpdateAction) {
        match action {
            UpdateAction::Check => self.check_for_updates(),
            UpdateAction::Install => self.install_update(),
            UpdateAction::Notes => {
                let notes = self
                    .0
                    .update_state
                    .borrow()
                    .release()
                    .map(Release::notes_url);
                match notes {
                    Some(notes) => about::open_link(&notes),
                    None => self.show_whats_new(),
                }
            }
        }
    }

    fn check_for_updates(&self) {
        let Some(updater) = &self.0.updater else {
            *self.0.update_state.borrow_mut() =
                State::CheckFailed(text::UPDATE_THREAD_MISSING.to_owned());
            self.show_update();
            return;
        };
        if self.0.update_state.borrow_mut().begin_check() {
            updater.check();
            self.poll_updates_soon();
        }
        self.show_update();
    }

    /// Asks before downloading, since installing closes hindsight. The portable copy opens the
    /// release's page instead, which has the zip.
    fn install_update(&self) {
        let Some(release) = self.0.update_state.borrow().release().cloned() else {
            return;
        };
        let (Installation::Installed(_), Some(updater)) = (&self.0.installation, &self.0.updater)
        else {
            about::open_link(&release.notes_url());
            return;
        };
        let answer = self.0.main_window.hwnd().MessageBox(
            &text::update_confirmation(release.version()),
            APP_NAME,
            co::MB::OKCANCEL | co::MB::ICONINFORMATION,
        );
        if answer != Ok(co::DLGID::OK) {
            return;
        }
        // The dialog ran the message loop, so the state may have moved on.
        let Some(release) = self.0.update_state.borrow_mut().begin_download() else {
            return;
        };
        updater.download(release);
        self.poll_updates_soon();
        self.show_update();
    }

    fn poll_updates_soon(&self) {
        let _ = self
            .0
            .main_window
            .hwnd()
            .SetTimer(UPDATE_POLL_TIMER, UPDATE_POLL_MS, None);
    }

    fn poll_updates(&self) {
        let Some(updater) = &self.0.updater else {
            return;
        };
        while let Some(done) = updater.finished() {
            match done {
                Done::Checked(found) => self.checked(found),
                Done::Downloaded(release, installer) => self.downloaded(release, installer),
            }
        }
        let busy = matches!(
            *self.0.update_state.borrow(),
            State::Checking | State::Downloading(_)
        );
        if !busy {
            let _ = self.0.main_window.hwnd().KillTimer(UPDATE_POLL_TIMER);
        }
        self.show_update();
    }

    fn checked(&self, found: Result<Check, CheckError>) {
        let found = found.map_err(|error| log::chain(&error).to_string());
        match &found {
            Ok(Check::Newer(release)) => log::info(text::update_found(release.version())),
            Ok(Check::Current) => {}
            Err(reason) => log::warning(text::update_check_log(reason)),
        }
        self.0.update_state.borrow_mut().checked(found);
    }

    /// Starts a downloaded installer and quits, so the installer can replace this copy.
    fn downloaded(&self, release: Release, installer: Result<Installer, DownloadError>) {
        let Installation::Installed(folder) = &self.0.installation else {
            *self.0.update_state.borrow_mut() = State::Available(release);
            return;
        };
        let launched = installer
            .map_err(|error| log::chain(&error).to_string())
            .and_then(|installer| {
                installer
                    .launch(folder)
                    .map_err(|error| log::chain(&error).to_string())
            });
        let next = match launched {
            Ok(Elevation::Started) => {
                log::info(text::update_installing(release.version()));
                self.0.quitting.set(true);
                self.0.main_window.close();
                return;
            }
            Ok(Elevation::Declined) => {
                log::info(text::UPDATE_DECLINED);
                State::Available(release)
            }
            Err(reason) => {
                log::warning(text::update_download_log(release.version(), &reason));
                State::UpdateFailed(release, reason)
            }
        };
        *self.0.update_state.borrow_mut() = next;
    }

    fn show_update(&self) {
        // Cloned so no borrow is held while the controls are updated.
        let state = self.0.update_state.borrow().clone();
        self.0.about.show_update(&state, &self.0.installation);
    }

    fn apply_look(&self) {
        self.apply_icon();
        layout::match_title_bar(self.0.main_window.hwnd(), &self.0.skin);
        let classic = !self.0.skin.is_themed();
        for frame in self.panes().into_iter().flat_map(Pane::frames) {
            frame.show_title_label(classic);
        }
        self.0.clips.apply_look(&self.0.skin);
        self.0.screenshots.apply_look(&self.0.skin);
        self.redraw();
    }

    fn apply_icon(&self) {
        let theme = self.0.skin.theme();
        window::set_icon(self.0.main_window.hwnd(), icon::resource(theme));
        if let Some(tray) = &*self.0.tray.borrow()
            && let Err(error) = tray.set_theme(theme)
        {
            log::warning(text::tray_unavailable(log::chain(&error)));
        }
    }

    fn widen(&self, delta: i32) {
        for window in self.tab_and_pages() {
            resize_width_by(window, delta);
        }
        resize_width_by(self.0.main_window.hwnd(), delta);
    }

    /// Sets the hotkeys aside while a hotkey field has the keyboard, so pressing either there
    /// enters it instead of saving a clip or a screenshot.
    fn on_hotkey_capture(&self, capture: Capture) {
        match capture {
            Capture::Began => {
                for action in Action::ALL {
                    hotkey::unregister(self.0.main_window.hwnd(), action);
                }
            }
            Capture::Ended => self.register_configured_hotkeys(),
        }
    }

    /// Switches to `config`'s hotkeys when both can be registered, and back to the configured
    /// ones otherwise.
    fn switch_hotkeys(&self, config: &Config) -> Result<(), RegisterError> {
        let [save, screenshot] = self.register_hotkeys(config);
        let result = save.and(screenshot);
        if result.is_err() {
            self.register_configured_hotkeys();
        }
        result
    }

    fn register_configured_hotkeys(&self) {
        let configured = self.0.config.borrow().clone();
        let _ = self.register_hotkeys(&configured);
    }

    /// Both are unregistered first, so that hotkeys trading places do not refuse each other.
    fn register_hotkeys(&self, config: &Config) -> [Result<(), RegisterError>; 2] {
        for action in Action::ALL {
            hotkey::unregister(self.0.main_window.hwnd(), action);
        }
        Action::ALL.map(|action| self.register_hotkey(action, config))
    }

    fn register_hotkey(&self, action: Action, config: &Config) -> Result<(), RegisterError> {
        let hotkey = match action {
            Action::Save => config.hotkey,
            Action::Screenshot => config.screenshot_hotkey,
        };
        let result = hotkey::register(self.0.main_window.hwnd(), action, hotkey);
        self.note_hotkey_error(action, result.as_ref().err().map(ToString::to_string));
        result
    }

    /// Records a hotkey error, which is shown on the Status page until the hotkey is registered.
    fn note_hotkey_error(&self, action: Action, error: Option<String>) {
        if let Some(error) = &error {
            log::warning(error);
        }
        self.0.hotkey_errors.borrow_mut()[action.index()] = error;
    }

    fn hotkey_error(&self) -> Option<String> {
        let errors = self.0.hotkey_errors.borrow();
        let shown: Vec<&str> = errors.iter().flatten().map(String::as_str).collect();
        (!shown.is_empty()).then(|| shown.join("\r\n"))
    }

    fn refresh(&self) {
        let status = self.0.worker.status();
        let folder = self.0.config.borrow().clip_directory();
        let hotkey_error = self.hotkey_error();
        let window_notices = [
            hotkey_error.as_deref(),
            self.0.tray_error.get().map(String::as_str),
        ];
        if self.0.status.update(&status, window_notices) {
            self.fit_window();
        }
        self.0
            .clips
            .update(&status, &folder, self.tab_in_view(CLIPS_TAB));
        self.0
            .screenshots
            .update(&status, &folder, self.tab_in_view(SCREENSHOTS_TAB));
        self.0.logs.update();
        self.0.settings.update_devices();
    }

    fn tab_in_view(&self, index: u32) -> bool {
        self.0.main_window.hwnd().IsWindowVisible()
            && self
                .0
                .tab
                .items()
                .selected()
                .is_some_and(|item| item.index() == index)
    }

    fn fit_window(&self) {
        let extra = self.0.status.extra().max(self.0.settings.extra());
        let delta = gui::dpi_y(extra - self.0.extra.replace(extra));
        if delta != 0 {
            for window in self.tab_and_pages() {
                resize_height_by(window, delta);
            }
            for pane in self.panes() {
                pane.grow(delta);
            }
            resize_height_by(self.0.main_window.hwnd(), delta);
        }
        // Moved controls keep stale pixels otherwise.
        self.redraw();
    }

    /// The tabs' pages, in tab order.
    fn panes(&self) -> [&dyn Pane; PANES] {
        [
            &self.0.status,
            &self.0.clips,
            &self.0.screenshots,
            &self.0.settings,
            &self.0.logs,
            &self.0.about,
        ]
    }

    fn tab_and_pages(&self) -> impl Iterator<Item = &w::HWND> {
        iter::once(self.0.tab.hwnd()).chain(self.panes().into_iter().map(|pane| pane.page().hwnd()))
    }

    fn redraw(&self) {
        if let Ok(client) = self.0.main_window.hwnd().GetClientRect() {
            let _ = self.0.main_window.hwnd().RedrawWindow(
                client,
                &w::HRGN::NULL,
                co::RDW::INVALIDATE | co::RDW::ERASE | co::RDW::ALLCHILDREN,
            );
        }
    }
}

fn application_window(width: i32, start_hidden: bool) -> gui::WindowMain {
    // A window created visible would flash before `--tray` hides it.
    let visibility = if start_hidden {
        co::WS::NoValue
    } else {
        co::WS::VISIBLE
    };
    gui::WindowMain::new(gui::WindowMainOpts {
        class_name: instance::WINDOW_CLASS,
        style: co::WS::CAPTION
            | co::WS::SYSMENU
            | co::WS::MINIMIZEBOX
            | co::WS::CLIPCHILDREN
            | co::WS::BORDER
            | visibility,
        ..layout::window_opts(width, WINDOW_HEIGHT, ICON_RESOURCE)
    })
}

fn open(path: &Path) {
    if let Err(error) = shell::open(path) {
        log::warning(text::open_failed(path, error));
    }
}

/// Returns the codecs the Settings tab offers: those the GPU encodes, or all of them when that
/// cannot be determined.
fn offered_codecs() -> Vec<Codec> {
    match gpu::supported_codecs() {
        Ok(codecs) if !codecs.is_empty() => {
            log::info(text::codecs_offered(&codecs));
            codecs
        }
        Ok(_) => {
            log::warning(text::codecs_unknown("the driver lists no codec"));
            Codec::ALL.to_vec()
        }
        Err(error) => {
            log::warning(text::codecs_unknown(error));
            Codec::ALL.to_vec()
        }
    }
}

fn startup_setting() -> (bool, Option<String>) {
    match startup::enabled() {
        Ok(enabled) => (enabled, None),
        Err(error) => (false, Some(text::startup_setting_unreadable(error))),
    }
}

/// A one-shot timer callback that destroys the main window once no winsafe handler is running.
extern "system" fn destroy_window(window: w::HWND, _: co::WM, id: usize, _: u32) {
    let _ = window.KillTimer(id);
    let _ = window.DestroyWindow();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// More than any character of Segoe UI 9 pt averages at 96 DPI, which is about 6 pixels.
    const CHARACTER_WIDTH: i32 = 7;
    /// The tab control's padding on both sides of a title, and the selected tab's overhang.
    const TAB_PADDING: i32 = 24;

    #[test]
    fn the_tab_titles_fit_across_the_narrowest_window_on_one_row() {
        let titles = [
            text::TAB_STATUS,
            text::TAB_CLIPS,
            text::TAB_SCREENSHOTS,
            text::TAB_SETTINGS,
            text::TAB_LOGS,
            text::TAB_ABOUT,
        ];
        assert_eq!(titles.len(), PANES);
        let strip: i32 = titles
            .iter()
            .map(|title| {
                layout::saturating_i32(title.chars().count()) * CHARACTER_WIDTH + TAB_PADDING
            })
            .sum();
        let tab_width = WINDOW_WIDTH - 2 * MARGIN;
        assert!(strip <= tab_width, "{strip} in {tab_width}");
    }
}
