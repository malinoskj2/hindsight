//! The desktop window: a plain Windows dialog made of the system's own common controls, so
//! fonts, tabs, group boxes and buttons are drawn by Windows exactly like any control panel.

use crate::{
    APP_NAME,
    config::{Backend, Config, Mode},
    log,
    service::Worker,
    sys::{
        foreground::{self, ForegroundHook},
        hotkey, startup,
        tray::{Tray, TrayAction},
        window,
    },
};
use anyhow::Result;
use footer::Footer;
use layout::{MARGIN, WINDOW_WIDTH, resize_by, resize_width_by, shift_by};
use logs::LogsPage;
use settings::SettingsPage;
use status::StatusPage;
use std::{cell::RefCell, rc::Rc};
use theme::Skin;
use winsafe::{self as w, co, gui, prelude::*};

mod art;
mod footer;
mod layout;
mod logs;
mod nvfbc_key;
mod settings;
mod startup_error;
mod status;
mod text;
mod theme;

const STATUS_TIMER: usize = 1;
const STATUS_INTERVAL_MS: u32 = 1000;
/// Resource id of the icon embedded by `build.rs`.
const ICON_RESOURCE: u16 = 1;

/// The tab and its margins; a footer row is added only while it has something to say.
const WINDOW_HEIGHT: i32 = MARGIN + TAB_HEIGHT + MARGIN;
const TAB_HEIGHT: i32 = 384;

/// Shows the application window, or the startup error, until the user quits.
pub(crate) fn run(startup: Result<(Worker, Config)>, start_hidden: bool) -> Result<()> {
    match startup {
        Ok((worker, config)) => Main::create_and_run(worker, config, start_hidden),
        Err(error) => startup_error::show(&format!("{error:#}")),
    }
}

/// What the handlers share. Every borrow is short-lived and none is taken while painting:
/// the colours the paint-time handlers read live beside their labels.
struct Shared {
    worker: Worker,
    config: Config,
    tray: Option<Tray>,
    /// Reports foreground changes while it lives.
    foreground_hook: Option<ForegroundHook>,
    quitting: bool,
    /// Why the save hotkey is not registered, while it is not.
    hotkey_error: Option<String>,
    /// A Run-key error found before the window existed, shown once it does.
    initial_startup_error: Option<String>,
    heights: Heights,
}

/// The height added to the fixed layout: the tab and its pages get the larger of the pages'
/// expanded groups, the window that plus the footer row once it is shown.
#[derive(Default)]
struct Heights {
    footer: i32,
    pages: i32,
    window: i32,
}

impl Heights {
    /// Records what the pages now need and returns how much the pages and the window must
    /// change, in logical pixels.
    fn fit(&mut self, pages: i32) -> (i32, i32) {
        let window = pages + self.footer;
        let deltas = (pages - self.pages, window - self.window);
        self.pages = pages;
        self.window = window;
        deltas
    }
}

#[derive(Clone)]
struct Main {
    wnd: gui::WindowMain,
    tab: gui::Tab,
    status: StatusPage,
    settings: SettingsPage,
    logs: LogsPage,
    footer: Footer,
    skin: Skin,
    shared: Rc<RefCell<Shared>>,
}

impl Main {
    fn create_and_run(worker: Worker, config: Config, start_hidden: bool) -> Result<()> {
        // A window created visible would flash before `--tray` hides it.
        let visibility = if start_hidden {
            co::WS::NoValue
        } else {
            co::WS::VISIBLE
        };
        // The look chosen last time; a character theme widens the window for its artwork.
        let skin = Skin::new(config.theme);
        let width = WINDOW_WIDTH + skin.extra_width();
        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: APP_NAME,
            size: gui::dpi(width, WINDOW_HEIGHT),
            class_icon: gui::Icon::Id(ICON_RESOURCE),
            style: co::WS::CAPTION
                | co::WS::SYSMENU
                | co::WS::MINIMIZEBOX
                | co::WS::CLIPCHILDREN
                | co::WS::BORDER
                | visibility,
            ..Default::default()
        });

        let (start_with_windows, startup_error) = match startup::enabled() {
            Ok(enabled) => (enabled, None),
            Err(error) => (false, Some(text::startup_setting_failed(error))),
        };
        let status = StatusPage::new(&wnd, &skin);
        let settings = SettingsPage::new(&wnd, &config, start_with_windows, &skin);
        let logs = LogsPage::new(&wnd, &skin);
        let tab = gui::Tab::new(
            &wnd,
            gui::TabOpts {
                position: gui::dpi(MARGIN, MARGIN),
                size: gui::dpi(width - 2 * MARGIN, TAB_HEIGHT),
                pages: &[
                    (text::TAB_STATUS, status.page().clone()),
                    (text::TAB_SETTINGS, settings.page().clone()),
                    (text::TAB_LOGS, logs.page().clone()),
                ],
                ..Default::default()
            },
        );
        // A character theme draws the strip of tabs itself; classic leaves it to Windows.
        let (tabs_skin, tabs) = (skin.clone(), tab.clone());
        tab.on_subclass().wm_paint(move || {
            if tabs_skin.is_themed() {
                tabs_skin.paint_tabs(&tabs);
            } else {
                theme::paint_tabs_natively(tabs.hwnd());
            }
            Ok(())
        });
        let footer = Footer::new(&wnd, &skin);

        let main = Self {
            wnd,
            tab,
            status,
            settings,
            logs,
            footer,
            skin,
            shared: Rc::new(RefCell::new(Shared {
                worker,
                config,
                tray: None,
                foreground_hook: None,
                quitting: false,
                hotkey_error: None,
                initial_startup_error: startup_error,
                heights: Heights::default(),
            })),
        };
        main.events();

        let show = if start_hidden {
            co::SW::HIDE
        } else {
            co::SW::SHOW
        };
        main.wnd
            .run_main(Some(show))
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("run the application window: {error}"))
    }

    fn events(&self) {
        let me = self.clone();
        self.wnd.on().wm_create(move |_| {
            me.on_create();
            Ok(0)
        });

        let me = self.clone();
        self.wnd.on().wm_show_window(move |p| {
            me.on_show_window(p.being_shown);
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_timer(STATUS_TIMER, move || {
            // The worker publishes what it asked for; the figures shown are from the last one.
            me.shared.borrow().worker.refresh();
            me.refresh();
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm(co::WM::HOTKEY, move |message| {
            if message.wparam == hotkey::SAVE_HOTKEY_ID.unsigned_abs() as usize {
                me.save();
            }
            Ok(0)
        });

        let me = self.clone();
        self.wnd.on().wm_close(move || {
            me.on_close();
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_destroy(move || {
            // Breaks the reference cycles through the tray's and the hook's callbacks so the
            // worker is dropped, which stops recording and finishes a clip being saved.
            let mut shared = me.shared.borrow_mut();
            shared.tray = None;
            shared.foreground_hook = None;
            Ok(())
        });

        let me = self.clone();
        self.status.save.on().bn_clicked(move || {
            me.save();
            Ok(())
        });

        let me = self.clone();
        self.status.recording.on().bn_clicked(move || {
            me.toggle_recording();
            Ok(())
        });

        let me = self.clone();
        self.settings.apply.on().bn_clicked(move || {
            me.apply_settings();
            Ok(())
        });

        let me = self.clone();
        self.settings.backend.on().cbn_sel_change(move || {
            me.on_backend_chosen();
            Ok(())
        });

        let me = self.clone();
        self.settings.theme.on().cbn_sel_change(move || {
            me.on_theme_chosen();
            Ok(())
        });

        for field in self.settings.estimate_inputs() {
            let me = self.clone();
            field.on().en_change(move || {
                me.settings.update_estimate();
                Ok(())
            });
        }

        let me = self.clone();
        self.settings.start_with_windows.on().bn_clicked(move || {
            if me.settings.toggle_startup() {
                me.fit_window();
            }
            Ok(())
        });
    }

    fn on_create(&self) {
        self.apply_look();
        self.status.on_create();
        self.settings.on_create();
        self.logs.on_create();
        self.footer.control().hwnd().ShowWindow(co::SW::HIDE);
        let initial_error = self.shared.borrow_mut().initial_startup_error.take();
        if let Some(error) = initial_error
            && self.settings.show_startup_error(Some(&error))
        {
            self.fit_window();
        }

        let configured = self.shared.borrow().config.clone();
        self.register_hotkey(&configured);

        let me = self.clone();
        let hook = ForegroundHook::install(move || me.watch_foreground());
        if hook.is_none() {
            log::warning(text::FOREGROUND_WATCH_FAILED);
        }
        self.shared.borrow_mut().foreground_hook = hook;
        self.watch_foreground();

        let me = self.clone();
        match Tray::new(move |action| me.on_tray(action)) {
            Ok(tray) => self.shared.borrow_mut().tray = Some(tray),
            Err(error) => {
                // Without a tray, closing quits; the footer row appears to say so.
                let message = text::tray_unavailable(error);
                log::warning(&message);
                self.footer.show_hint(&message);
                self.shared.borrow_mut().heights.footer = footer::HEIGHT;
                self.fit_window();
            }
        }
        self.refresh();
    }

    /// The status refresh runs only while the window can be seen: Windows sends this for the
    /// visible creation, for hiding to the tray and for every later `ShowWindow` that changes
    /// visibility, so a window parked in the tray wakes nothing.
    fn on_show_window(&self, being_shown: bool) {
        if being_shown {
            let _ = self
                .wnd
                .hwnd()
                .SetTimer(STATUS_TIMER, STATUS_INTERVAL_MS, None);
            self.refresh();
        } else {
            let _ = self.wnd.hwnd().KillTimer(STATUS_TIMER);
        }
    }

    fn on_tray(&self, action: TrayAction) {
        match action {
            TrayAction::Show => self.show(),
            TrayAction::Save => self.save(),
            TrayAction::Quit => {
                log::info(text::QUIT_FROM_TRAY);
                self.shared.borrow_mut().quitting = true;
                self.on_close();
            }
        }
    }

    fn show(&self) {
        self.wnd.hwnd().ShowWindow(co::SW::RESTORE);
        self.wnd.hwnd().SetForegroundWindow();
    }

    /// Closing hides to the tray; quitting (or closing without a tray) destroys the window,
    /// and with it the worker, which stops recording first.
    fn on_close(&self) {
        let shared = self.shared.borrow();
        if shared.tray.is_some() && !shared.quitting {
            drop(shared);
            log::info(text::CLOSED_TO_TRAY);
            self.wnd.hwnd().ShowWindow(co::SW::HIDE);
            return;
        }
        drop(shared);
        let _ = self.wnd.hwnd().DestroyWindow();
    }

    /// Tells the worker about the full-screen app in front, if the mode records those. The
    /// worker ignores an app it is already recording.
    fn watch_foreground(&self) {
        let shared = self.shared.borrow();
        if shared.config.mode == Mode::Fullscreen
            && let Some(target) = foreground::fullscreen_app()
        {
            shared.worker.target(target);
        }
    }

    fn save(&self) {
        self.shared.borrow().worker.save();
        if self.wnd.hwnd().IsWindowVisible() {
            self.refresh();
        }
    }

    fn toggle_recording(&self) {
        let shared = self.shared.borrow();
        let status = shared.worker.status();
        if status.recording || status.state == crate::service::DisplayStatus::Waiting {
            shared.worker.stop();
        } else {
            shared.worker.start(shared.config.clone());
            drop(shared);
            self.watch_foreground();
        }
    }

    fn apply_settings(&self) {
        let config = match self.settings.configuration() {
            Ok(config) => config,
            Err(error) => {
                self.settings.show_settings_error(&error);
                return;
            }
        };
        if !self.register_hotkey(&config) {
            let error = self
                .shared
                .borrow()
                .hotkey_error
                .clone()
                .unwrap_or_default();
            self.settings.show_settings_error(&error);
            // The previous hotkey stays in effect.
            let previous = self.shared.borrow().config.clone();
            self.register_hotkey(&previous);
            return;
        }
        self.settings.show_settings_error("");
        self.settings.show_hotkey(&config.hotkey);
        let mut shared = self.shared.borrow_mut();
        shared.config = config.clone();
        shared.worker.start(config);
        drop(shared);
        self.watch_foreground();
    }

    /// Choosing NvFBC asks for the key NVIDIA issued, as NVIDIA's own setup does; without one
    /// the previous choice comes back. The choice takes effect on Apply, like every setting.
    fn on_backend_chosen(&self) {
        let accepted =
            self.settings.chosen_backend() != Backend::Nvfbc || nvfbc_key::ask(&self.wnd);
        self.settings.settle_backend(accepted);
    }

    /// A new look was chosen in the Settings tab: it shows at once and is remembered, and
    /// recording carries on as it was.
    fn on_theme_chosen(&self) {
        let theme = self.settings.chosen_theme();
        let before = self.skin.extra_width();
        if theme == self.skin.theme() {
            return;
        }
        self.skin.set(theme);
        let delta = gui::dpi_x(self.skin.extra_width() - before);
        if delta != 0 {
            for window in [
                self.tab.hwnd(),
                self.status.page().hwnd(),
                self.settings.page().hwnd(),
                self.logs.page().hwnd(),
            ] {
                resize_width_by(window, delta);
            }
            resize_width_by(self.wnd.hwnd(), delta);
        }
        let mut shared = self.shared.borrow_mut();
        shared.config.theme = theme;
        shared.worker.set_theme(theme);
        drop(shared);
        self.apply_look();
    }

    /// Dresses the window in the chosen look: the title bar, the group titles, and a redraw of
    /// everything.
    fn apply_look(&self) {
        match theme::title_bar_colours(&self.skin) {
            Some((caption, text, _)) => window::paint_title_bar(self.wnd.hwnd(), caption, text),
            None => window::match_dialog_title_bar(self.wnd.hwnd()),
        }
        // Classic shows the titles as labels; a character theme paints them on its panels.
        let classic = !self.skin.is_themed();
        for frame in self.status.frames().iter().chain(self.settings.frames()) {
            frame.show_title_label(classic);
        }
        if let Ok(client) = self.wnd.hwnd().GetClientRect() {
            let _ = self.wnd.hwnd().RedrawWindow(
                client,
                &w::HRGN::NULL,
                co::RDW::INVALIDATE | co::RDW::ERASE | co::RDW::ALLCHILDREN,
            );
        }
    }

    /// Makes the configured hotkey save a clip; reports whether it could.
    fn register_hotkey(&self, config: &Config) -> bool {
        let result = config
            .parsed_hotkey()
            .map_err(|error| error.to_string())
            .and_then(|parsed| {
                hotkey::register(self.wnd.hwnd(), parsed).map_err(|error| error.to_string())
            });
        let registered = result.is_ok();
        if let Err(error) = &result {
            log::warning(error);
        }
        self.shared.borrow_mut().hotkey_error = result.err();
        registered
    }

    fn refresh(&self) {
        let (status, config, hotkey_error) = {
            let shared = self.shared.borrow();
            (
                shared.worker.status(),
                shared.config.clone(),
                shared.hotkey_error.clone(),
            )
        };
        if self
            .status
            .update(&status, &config, hotkey_error.as_deref())
        {
            self.fit_window();
        }
        self.logs.update();
    }

    /// Gives the tab and the pages the larger of the pages' extra heights, moving the footer
    /// along; the window gets that plus the footer row when it is shown. Repaints.
    fn fit_window(&self) {
        let pages = self.status.extra().max(self.settings.extra());
        let (page_delta, window_delta) = self.shared.borrow_mut().heights.fit(pages);
        let (page_delta, window_delta) = (gui::dpi_y(page_delta), gui::dpi_y(window_delta));
        if page_delta != 0 {
            for window in [
                self.tab.hwnd(),
                self.status.page().hwnd(),
                self.settings.page().hwnd(),
                self.logs.page().hwnd(),
                self.logs.view().hwnd(),
            ] {
                resize_by(window, page_delta);
            }
            shift_by(self.footer.control().hwnd(), page_delta);
        }
        if window_delta != 0 {
            resize_by(self.wnd.hwnd(), window_delta);
        }
        // Moved controls and the strip a shorter window uncovers keep stale pixels otherwise.
        if let Ok(client) = self.wnd.hwnd().GetClientRect() {
            let _ = self.wnd.hwnd().RedrawWindow(
                client,
                &w::HRGN::NULL,
                co::RDW::INVALIDATE | co::RDW::ERASE | co::RDW::ALLCHILDREN,
            );
        }
    }
}
