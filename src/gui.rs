//! The main window: a plain Win32 dialog built from the system's common controls.

use self::{
    clips::ClipsPage,
    layout::{MARGIN, TAB_HEIGHT, WINDOW_HEIGHT, WINDOW_WIDTH, resize_height_by, resize_width_by},
    logs::LogsPage,
    settings::SettingsPage,
    status::{Control, StatusPage},
    theme::{Skin, Surface},
};
use crate::{
    config::{Codec, Config, Mode, Theme},
    gpu, log,
    service::{DisplayStatus, Worker},
    sys::{
        foreground::{self, ForegroundHook},
        hotkey::{self, Hotkey},
        icon, instance, shell, startup,
        tray::{Tray, TrayAction},
        window,
    },
};
use anyhow::Result;
use std::{
    cell::{Cell, OnceCell, RefCell},
    fs,
    path::Path,
    rc::Rc,
};
use winsafe::{self as w, co, gui, prelude::*};

mod art;
mod clips;
mod layout;
mod logs;
mod settings;
mod startup_error;
mod status;
mod text;
mod theme;

/// The Classic icon: the window class and the error window; themed windows set their own.
const ICON_RESOURCE: u16 = icon::resource(Theme::Classic);
const STATUS_TIMER: usize = 1;
const STATUS_INTERVAL_MS: u32 = 1000;
const CLIPS_TAB: u32 = 1;

pub(crate) fn run(startup: Result<(Worker, Config)>, start_hidden: bool) -> Result<()> {
    match startup {
        Ok((worker, config)) => Main::create_and_run(worker, config, start_hidden),
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
    clips: ClipsPage,
    settings: SettingsPage,
    logs: LogsPage,
    skin: Skin,
    worker: Worker,
    config: RefCell<Config>,
    tray: RefCell<Option<Tray>>,
    foreground_hook: RefCell<Option<ForegroundHook>>,
    quitting: Cell<bool>,
    hotkey_error: RefCell<Option<String>>,
    tray_error: OnceCell<String>,
    initial_startup_error: Cell<Option<String>>,
    /// By how much the pages have grown for their messages.
    extra: Cell<i32>,
}

impl Main {
    fn create_and_run(worker: Worker, config: Config, start_hidden: bool) -> Result<()> {
        let main = Self::create(worker, config, start_hidden);
        main.draw();
        main.window_events();
        main.status_events();
        main.settings_events();
        main.run(start_hidden)
    }

    fn create(worker: Worker, config: Config, start_hidden: bool) -> Self {
        let skin = Skin::new(config.theme);
        let width = WINDOW_WIDTH + skin.extra_width();
        let main_window = application_window(width, start_hidden);
        let (start_with_windows, startup_error) = startup_setting();
        let status = StatusPage::new(&main_window, &skin);
        let clips = ClipsPage::new(&main_window, &skin);
        let codecs = offered_codecs();
        let settings = SettingsPage::new(&main_window, &config, &codecs, start_with_windows, &skin);
        let logs = LogsPage::new(&main_window, &skin);
        let tab = gui::Tab::new(
            &main_window,
            gui::TabOpts {
                position: gui::dpi(MARGIN, MARGIN),
                size: gui::dpi(width - 2 * MARGIN, TAB_HEIGHT),
                pages: &[
                    (text::TAB_STATUS, status.page().clone()),
                    (text::TAB_CLIPS, clips.page().clone()),
                    (text::TAB_SETTINGS, settings.page().clone()),
                    (text::TAB_LOGS, logs.page().clone()),
                ],
                ..Default::default()
            },
        );
        Self(Rc::new(Inner {
            main_window,
            tab,
            status,
            clips,
            settings,
            logs,
            skin,
            worker,
            config: RefCell::new(config),
            tray: RefCell::default(),
            foreground_hook: RefCell::default(),
            quitting: Cell::new(false),
            hotkey_error: RefCell::default(),
            tray_error: OnceCell::new(),
            initial_startup_error: Cell::new(startup_error),
            extra: Cell::new(0),
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
                if self.0.worker.status().state == DisplayStatus::Paused {
                    self.0.worker.resume();
                    self.watch_foreground();
                } else {
                    self.0.worker.pause();
                }
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
                    log::warning(text::clip_open_failed(&folder, error));
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
        self.0.status.on_create();
        self.0.clips.on_create();
        self.0.settings.on_create();
        self.0.logs.on_create();
        self.show_initial_startup_error();
        self.register_configured_hotkey();
        self.watch_foreground_changes();
        self.add_tray_icon();
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
                let message = text::tray_unavailable(error);
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
            self.refresh();
        } else {
            let _ = self.0.main_window.hwnd().KillTimer(STATUS_TIMER);
        }
    }

    fn on_timer(&self) {
        self.0.worker.refresh();
        self.refresh();
    }

    fn on_hotkey(&self, id: usize) {
        if usize::try_from(hotkey::SAVE_HOTKEY_ID) == Ok(id) {
            self.save();
        }
    }

    fn show(&self) {
        self.0.main_window.hwnd().ShowWindow(co::SW::RESTORE);
        self.0.main_window.hwnd().SetForegroundWindow();
    }

    fn on_close(&self) {
        let to_tray = self.0.tray.borrow().is_some() && !self.0.quitting.get();
        if to_tray {
            log::info(text::CLOSED_TO_TRAY);
            self.0.main_window.hwnd().ShowWindow(co::SW::HIDE);
            return;
        }
        // DestroyWindow frees this handler: go through a stack copy and touch nothing after.
        let main_window = self.0.main_window.clone();
        let _ = main_window.hwnd().DestroyWindow();
    }

    fn on_destroy(&self) {
        // Dropped outside the borrows: the tray's teardown can re-enter and borrow again.
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
                self.on_close();
            }
        }
    }

    fn watch_foreground(&self) {
        let fullscreen = self.0.config.borrow().mode == Mode::Fullscreen;
        if fullscreen && let Some(target) = foreground::fullscreen_app() {
            self.0.worker.target(target);
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
                self.0.settings.show_settings_error(&error);
                return;
            }
        };
        if let Err(error) = self.switch_hotkey(config.hotkey) {
            self.0.settings.show_settings_error(&error);
            return;
        }
        self.0.settings.show_settings_error("");
        self.0.settings.show_hotkey(&config.hotkey.to_string());
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
        self.0.config.borrow_mut().theme = theme;
        self.0.worker.set_theme(theme);
        self.apply_look();
    }

    fn apply_look(&self) {
        self.apply_icon();
        let hwnd = self.0.main_window.hwnd();
        match self.0.skin.title_bar_colours() {
            Some((caption, text)) => window::paint_title_bar(hwnd, caption, text),
            None => window::match_dialog_title_bar(hwnd),
        }
        let classic = !self.0.skin.is_themed();
        for frame in self
            .0
            .status
            .frames()
            .iter()
            .chain(self.0.settings.frames())
        {
            frame.show_title_label(classic);
        }
        self.0.clips.apply_look(&self.0.skin);
        self.redraw();
    }

    fn apply_icon(&self) {
        let theme = self.0.skin.theme();
        window::set_icon(self.0.main_window.hwnd(), icon::resource(theme));
        if let Some(tray) = &*self.0.tray.borrow()
            && let Err(error) = tray.set_theme(theme)
        {
            log::warning(text::tray_unavailable(error));
        }
    }

    fn widen(&self, delta: i32) {
        for window in self.tab_and_pages() {
            resize_width_by(window, delta);
        }
        resize_width_by(self.0.main_window.hwnd(), delta);
    }

    fn switch_hotkey(&self, hotkey: Hotkey) -> Result<(), String> {
        let result = self.register_hotkey(hotkey);
        if result.is_err() {
            self.register_configured_hotkey();
        }
        result
    }

    fn register_configured_hotkey(&self) {
        let configured = self.0.config.borrow().hotkey;
        let _ = self.register_hotkey(configured);
    }

    fn register_hotkey(&self, hotkey: Hotkey) -> Result<(), String> {
        let result =
            hotkey::register(self.0.main_window.hwnd(), hotkey).map_err(|error| error.to_string());
        self.note_hotkey_error(result.as_ref().err().cloned());
        result
    }

    /// Shown on the Status page until the hotkey is registered.
    fn note_hotkey_error(&self, error: Option<String>) {
        if let Some(error) = &error {
            log::warning(error);
        }
        *self.0.hotkey_error.borrow_mut() = error;
    }

    fn refresh(&self) {
        let status = self.0.worker.status();
        let folder = self.0.config.borrow().clip_directory();
        let hotkey_error = self.0.hotkey_error.borrow().clone();
        let window_notices = [
            hotkey_error.as_deref(),
            self.0.tray_error.get().map(String::as_str),
        ];
        if self.0.status.update(&status, window_notices) {
            self.fit_window();
        }
        self.0.clips.update(&status, &folder, self.clips_in_view());
        self.0.logs.update();
    }

    fn clips_in_view(&self) -> bool {
        self.0.main_window.hwnd().IsWindowVisible()
            && self
                .0
                .tab
                .items()
                .selected()
                .is_some_and(|item| item.index() == CLIPS_TAB)
    }

    fn fit_window(&self) {
        let extra = self.0.status.extra().max(self.0.settings.extra());
        let delta = gui::dpi_y(extra - self.0.extra.replace(extra));
        if delta != 0 {
            for window in self.tab_and_pages() {
                resize_height_by(window, delta);
            }
            self.0.clips.grow(delta);
            self.0.logs.grow(delta);
            resize_height_by(self.0.main_window.hwnd(), delta);
        }
        // Moved controls keep stale pixels otherwise.
        self.redraw();
    }

    fn tab_and_pages(&self) -> [&w::HWND; 5] {
        [
            self.0.tab.hwnd(),
            self.0.status.page().hwnd(),
            self.0.clips.page().hwnd(),
            self.0.settings.page().hwnd(),
            self.0.logs.page().hwnd(),
        ]
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
        log::warning(text::clip_open_failed(path, error));
    }
}

/// The codecs the Settings tab offers: those the GPU encodes, or all of them when it cannot say.
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
