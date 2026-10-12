//! The desktop window: a plain Windows dialog made of the system's own common controls, so
//! fonts, tabs, group boxes and buttons are drawn by Windows exactly like any control panel.

use self::{
    clips::ClipsPage,
    footer::Footer,
    layout::{BLACK, GRAY, MARGIN, WINDOW_WIDTH, resize_height_by, resize_width_by, shift_by},
    logs::LogsPage,
    settings::SettingsPage,
    status::StatusPage,
    theme::{Skin, Surface},
};
use crate::{
    APP_NAME,
    config::{Config, Mode},
    log,
    service::Worker,
    sys::{
        foreground::{self, ForegroundHook},
        hotkey, instance, startup,
        tray::{Tray, TrayAction},
        window,
    },
};
use anyhow::Result;
use std::{cell::RefCell, rc::Rc};
use winsafe::{self as w, co, gui, prelude::*};

mod art;
mod clips;
mod footer;
mod layout;
mod logs;
mod nvfbc_key;
mod settings;
mod startup_error;
mod status;
mod text;
mod theme;

/// Resource id of the icon embedded by `build.rs`.
const ICON_RESOURCE: u16 = 1;
/// Id of the timer that refreshes the pages while the window can be seen.
const STATUS_TIMER: usize = 1;
/// How often the pages refresh, in milliseconds.
const STATUS_INTERVAL_MS: u32 = 1000;
/// Where the Clips tab stands in the strip.
const CLIPS_TAB: u32 = 1;

/// Height of the tab control, in logical pixels.
const TAB_HEIGHT: i32 = 406;
/// The tab and its margins; a footer row is added only while it has something to say.
const WINDOW_HEIGHT: i32 = MARGIN + TAB_HEIGHT + MARGIN;

/// Shows the application window, or the startup error, until the user quits.
pub(crate) fn run(startup: Result<(Worker, Config)>, start_hidden: bool) -> Result<()> {
    match startup {
        Ok((worker, config)) => Main::create_and_run(worker, config, start_hidden),
        Err(error) => startup_error::show(&format!("{error:#}")),
    }
}

/// The application window, its tab of pages and footer row, the look they are drawn in, and
/// the state the handlers share.
#[derive(Clone)]
struct Main {
    wnd: gui::WindowMain,
    tab: gui::Tab,
    status: StatusPage,
    clips: ClipsPage,
    settings: SettingsPage,
    logs: LogsPage,
    footer: Footer,
    skin: Skin,
    shared: Rc<RefCell<Shared>>,
}

impl Main {
    /// Declares the window, wires it up and runs it until the user quits.
    fn create_and_run(worker: Worker, config: Config, start_hidden: bool) -> Result<()> {
        let main = Self::create(worker, config, start_hidden);
        main.draw();
        main.window_events();
        main.settings_events();
        main.run(start_hidden)
    }

    /// Declares the window and every control on it, in tab order, and the state the handlers
    /// share; Windows creates them all when the window runs.
    fn create(worker: Worker, config: Config, start_hidden: bool) -> Self {
        // The look chosen last time; a character theme widens the window for its artwork.
        let skin = Skin::new(config.theme);
        let width = WINDOW_WIDTH + skin.extra_width();
        let wnd = main_window(width, start_hidden);
        let (start_with_windows, startup_error) = startup_setting();
        let status = StatusPage::new(&wnd, &skin);
        let clips = ClipsPage::new(&wnd, &skin);
        let settings = SettingsPage::new(&wnd, &config, start_with_windows, &skin);
        let logs = LogsPage::new(&wnd, &skin);
        let tab = gui::Tab::new(
            &wnd,
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
        let footer = Footer::new(&wnd);
        Self {
            wnd,
            tab,
            status,
            clips,
            settings,
            logs,
            footer,
            skin,
            shared: Rc::new(RefCell::new(Shared::new(worker, config, startup_error))),
        }
    }

    /// Installs the handlers that draw the window's own parts and its strip of tabs; the pages
    /// install their own.
    fn draw(&self) {
        let skin = self.skin.clone();
        let tab = self.tab.clone();
        self.tab.on_subclass().wm_paint(move || {
            if skin.is_themed() {
                skin.paint_tabs(&tab);
            } else {
                theme::paint_tabs_natively(tab.hwnd());
            }
            Ok(())
        });
        // The footer's hint is muted; the window has no other labels.
        let footer = self.footer.clone();
        layout::draw(
            &self.wnd,
            self.wnd.on(),
            Vec::new(),
            &self.skin,
            Surface::Dialog,
            move |hwnd| {
                if *hwnd == *footer.control().hwnd() {
                    GRAY
                } else {
                    BLACK
                }
            },
        );
    }

    /// Wires the window's own messages: creation, visibility, the status timer, a change of
    /// tab, the hotkey, another launch, closing and destruction.
    fn window_events(&self) {
        let me = self.clone();
        self.wnd.on().wm_create(move |_| {
            me.on_create();
            Ok(0)
        });

        // The Clips tab reads its folder when it comes into view, not before.
        let me = self.clone();
        self.tab.on().tcn_sel_change(move || {
            me.refresh();
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_show_window(move |p| {
            me.on_show_window(p.being_shown);
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_timer(STATUS_TIMER, move || {
            me.on_timer();
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm(co::WM::HOTKEY, move |message| {
            me.on_hotkey(message.wparam);
            Ok(0)
        });

        // Another launch of the app: this window is the one to show.
        let me = self.clone();
        self.wnd.on().wm(instance::show_message(), move |_| {
            me.show();
            Ok(0)
        });

        let me = self.clone();
        self.wnd.on().wm_close(move || {
            me.on_close();
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_destroy(move || {
            me.on_destroy();
            Ok(())
        });
    }

    /// Wires the Settings tab's controls whose effect reaches beyond the page: Apply, the
    /// choices that act at once, and the Run-key switch. The page wires its own estimate.
    fn settings_events(&self) {
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

        let me = self.clone();
        self.settings.start_with_windows.on().bn_clicked(move || {
            if me.settings.toggle_startup() {
                me.fit_window();
            }
            Ok(())
        });

        self.settings.estimate_events();
    }

    /// Runs the window until it is destroyed, shown at once unless it starts in the tray.
    fn run(&self, start_hidden: bool) -> Result<()> {
        let show = if start_hidden {
            co::SW::HIDE
        } else {
            co::SW::SHOW
        };
        self.wnd
            .run_main(Some(show))
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("run the application window: {error}"))
    }

    /// Finishes the window once its controls exist: the look, the pages, the hotkey, the
    /// foreground hook and the tray icon, then a first refresh.
    fn on_create(&self) {
        self.apply_look();
        self.status.on_create();
        self.clips.on_create();
        self.settings.on_create();
        self.logs.on_create();
        self.footer.on_create();
        self.show_initial_startup_error();
        // A hotkey that cannot be registered is logged and shown on the Status tab.
        let configured = self.shared.borrow().config.clone();
        let _ = self.register_hotkey(&configured);
        self.watch_foreground_changes();
        self.add_tray_icon();
        self.refresh();
    }

    /// Shows the Run-key error found before the window existed, if there was one.
    fn show_initial_startup_error(&self) {
        let initial_error = self.shared.borrow_mut().initial_startup_error.take();
        if let Some(error) = initial_error
            && self.settings.show_startup_error(Some(&error))
        {
            self.fit_window();
        }
    }

    /// Hooks foreground changes so a full-screen app is noticed at once, and looks at the app
    /// in front now.
    fn watch_foreground_changes(&self) {
        let me = self.clone();
        let hook = ForegroundHook::install(move || me.watch_foreground());
        if hook.is_none() {
            log::warning(text::FOREGROUND_WATCH_FAILED);
        }
        self.shared.borrow_mut().foreground_hook = hook;
        self.watch_foreground();
    }

    /// Adds the notification-area icon, or shows the footer row saying closing will quit.
    fn add_tray_icon(&self) {
        let me = self.clone();
        match Tray::new(move |action| me.on_tray(action)) {
            Ok(tray) => self.shared.borrow_mut().tray = Some(tray),
            Err(error) => {
                let message = text::tray_unavailable(error);
                log::warning(&message);
                self.footer.show_hint(&message);
                self.shared.borrow_mut().heights.footer = footer::HEIGHT;
                self.fit_window();
            }
        }
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

    /// Asks the worker for fresh figures and shows the ones it published last.
    fn on_timer(&self) {
        self.shared.borrow().worker.refresh();
        self.refresh();
    }

    /// Saves a clip when the hotkey pressed, identified by `id`, is the save hotkey.
    fn on_hotkey(&self, id: usize) {
        if usize::try_from(hotkey::SAVE_HOTKEY_ID) == Ok(id) {
            self.save();
        }
    }

    /// Brings the window back from the tray or from behind other windows.
    fn show(&self) {
        self.wnd.hwnd().ShowWindow(co::SW::RESTORE);
        self.wnd.hwnd().SetForegroundWindow();
    }

    /// Closing hides to the tray; quitting (or closing without a tray) destroys the window,
    /// and with it the worker, which stops recording first.
    fn on_close(&self) {
        let to_tray = {
            let shared = self.shared.borrow();
            shared.tray.is_some() && !shared.quitting
        };
        if to_tray {
            log::info(text::CLOSED_TO_TRAY);
            self.wnd.hwnd().ShowWindow(co::SW::HIDE);
            return;
        }
        // Destroying the window frees its handlers, the one running this included, so the
        // call goes through a copy on the stack and nothing touches `self` after it.
        let wnd = self.wnd.clone();
        let _ = wnd.hwnd().DestroyWindow();
    }

    /// Breaks the reference cycles through the tray's and the hook's callbacks so the worker
    /// is dropped, which stops recording and finishes a clip being saved.
    fn on_destroy(&self) {
        // Dropped outside the borrow: the tray destroys a window of its own as it goes, and
        // anything that fires from that must find the shared state free.
        let (tray, hook) = {
            let mut shared = self.shared.borrow_mut();
            (shared.tray.take(), shared.foreground_hook.take())
        };
        drop(tray);
        drop(hook);
    }

    /// Does what was chosen from the tray icon's menu.
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

    /// Saves a clip of the replay buffer, showing the save under way if the window is visible.
    fn save(&self) {
        self.shared.borrow().worker.save();
        if self.wnd.hwnd().IsWindowVisible() {
            self.refresh();
        }
    }

    /// Applies the settings as entered, or shows the problem with them: the hotkey changes,
    /// and recording restarts with the new configuration.
    fn apply_settings(&self) {
        let config = match self.settings.configuration() {
            Ok(config) => config,
            Err(error) => {
                self.settings.show_settings_error(&error);
                return;
            }
        };
        if let Err(error) = self.switch_hotkey(&config) {
            self.settings.show_settings_error(&error);
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

    /// Choosing either NvFBC backend asks for the key NVIDIA issued, as NVIDIA's own setup
    /// does; without one the previous choice comes back. The choice takes effect on Apply,
    /// like every setting.
    fn on_backend_chosen(&self) {
        let accepted = !self.settings.chosen_backend().uses_nvfbc() || nvfbc_key::ask(&self.wnd);
        self.settings.settle_backend(accepted);
    }

    /// A new look was chosen in the Settings tab: it shows at once and is remembered, and
    /// recording carries on as it was.
    fn on_theme_chosen(&self) {
        let theme = self.settings.chosen_theme();
        if theme == self.skin.theme() {
            return;
        }
        let before = self.skin.extra_width();
        self.skin.set(theme);
        let delta = gui::dpi_x(self.skin.extra_width() - before);
        if delta != 0 {
            self.widen(delta);
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
        match self.skin.title_bar_colours() {
            Some((caption, text)) => window::paint_title_bar(self.wnd.hwnd(), caption, text),
            None => window::match_dialog_title_bar(self.wnd.hwnd()),
        }
        // Classic shows the titles as labels; a character theme paints them on its panels.
        let classic = !self.skin.is_themed();
        for frame in self.status.frames().iter().chain(self.settings.frames()) {
            frame.show_title_label(classic);
        }
        self.clips.apply_look(&self.skin);
        self.redraw();
    }

    /// Widens the window, the tab and its pages by `delta` physical pixels (narrows them when
    /// negative).
    fn widen(&self, delta: i32) {
        for window in self.tab_and_pages() {
            resize_width_by(window, delta);
        }
        resize_width_by(self.wnd.hwnd(), delta);
    }

    /// Makes the hotkey of `config` save a clip in place of the current one, which stays in
    /// effect when the new one cannot be registered.
    fn switch_hotkey(&self, config: &Config) -> Result<(), String> {
        let result = self.register_hotkey(config);
        if result.is_err() {
            let previous = self.shared.borrow().config.clone();
            let _ = self.register_hotkey(&previous);
        }
        result
    }

    /// Makes the configured hotkey save a clip, or logs and remembers why it could not.
    fn register_hotkey(&self, config: &Config) -> Result<(), String> {
        let result = config
            .parsed_hotkey()
            .map_err(|error| error.to_string())
            .and_then(|parsed| {
                hotkey::register(self.wnd.hwnd(), parsed).map_err(|error| error.to_string())
            });
        if let Err(error) = &result {
            log::warning(error);
        }
        self.shared.borrow_mut().hotkey_error = result.clone().err();
        result
    }

    /// Shows the worker's latest status on the Status tab, the clip folder on the Clips tab
    /// while it is in view, and new journal entries on the Logs tab, refitting the window when
    /// the Status tab changed height.
    fn refresh(&self) {
        let (status, config, hotkey_error) = {
            let shared = self.shared.borrow();
            (
                shared.worker.status(),
                shared.config.clone(),
                shared.hotkey_error.clone(),
            )
        };
        if self.status.update(&status, hotkey_error.as_deref()) {
            self.fit_window();
        }
        self.clips
            .update(&status, &config.clip_directory(), self.clips_in_view());
        self.logs.update();
    }

    /// Whether the Clips tab is the one showing in a window that can be seen.
    fn clips_in_view(&self) -> bool {
        self.wnd.hwnd().IsWindowVisible()
            && self
                .tab
                .items()
                .selected()
                .is_some_and(|item| item.index() == CLIPS_TAB)
    }

    /// Gives the tab and the pages the larger of the pages' extra heights, moving the footer
    /// along; the window gets that plus the footer row when it is shown. Repaints.
    fn fit_window(&self) {
        let pages = self.status.extra().max(self.settings.extra());
        let (page_delta, window_delta) = self.shared.borrow_mut().heights.fit(pages);
        let (page_delta, window_delta) = (gui::dpi_y(page_delta), gui::dpi_y(window_delta));
        if page_delta != 0 {
            for window in self.tab_and_pages() {
                resize_height_by(window, page_delta);
            }
            resize_height_by(self.clips.list().hwnd(), page_delta);
            resize_height_by(self.logs.view().hwnd(), page_delta);
            shift_by(self.footer.control().hwnd(), page_delta);
        }
        if window_delta != 0 {
            resize_height_by(self.wnd.hwnd(), window_delta);
        }
        // Moved controls and the strip a shorter window uncovers keep stale pixels otherwise.
        self.redraw();
    }

    /// The tab control and its pages, which share one size.
    fn tab_and_pages(&self) -> [&w::HWND; 5] {
        [
            self.tab.hwnd(),
            self.status.page().hwnd(),
            self.clips.page().hwnd(),
            self.settings.page().hwnd(),
            self.logs.page().hwnd(),
        ]
    }

    /// Repaints the whole window, every control included.
    fn redraw(&self) {
        if let Ok(client) = self.wnd.hwnd().GetClientRect() {
            let _ = self.wnd.hwnd().RedrawWindow(
                client,
                &w::HRGN::NULL,
                co::RDW::INVALIDATE | co::RDW::ERASE | co::RDW::ALLCHILDREN,
            );
        }
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

impl Shared {
    /// What the handlers share before the window exists: no tray, hook or hotkey yet.
    fn new(worker: Worker, config: Config, initial_startup_error: Option<String>) -> Self {
        Self {
            worker,
            config,
            tray: None,
            foreground_hook: None,
            quitting: false,
            hotkey_error: None,
            initial_startup_error,
            heights: Heights::default(),
        }
    }
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

/// The main window, `width` logical pixels across, declared hidden when it starts in the tray.
fn main_window(width: i32, start_hidden: bool) -> gui::WindowMain {
    // A window created visible would flash before `--tray` hides it.
    let visibility = if start_hidden {
        co::WS::NoValue
    } else {
        co::WS::VISIBLE
    };
    gui::WindowMain::new(gui::WindowMainOpts {
        title: APP_NAME,
        // A later launch finds this window by its class and asks it to come forward.
        class_name: instance::WINDOW_CLASS,
        size: gui::dpi(width, WINDOW_HEIGHT),
        class_icon: gui::Icon::Id(ICON_RESOURCE),
        style: co::WS::CAPTION
            | co::WS::SYSMENU
            | co::WS::MINIMIZEBOX
            | co::WS::CLIPCHILDREN
            | co::WS::BORDER
            | visibility,
        ..Default::default()
    })
}

/// Whether the app starts with Windows, and the message to show once the window exists when
/// the Run key could not be read.
fn startup_setting() -> (bool, Option<String>) {
    match startup::enabled() {
        Ok(enabled) => (enabled, None),
        Err(error) => (false, Some(text::startup_setting_unreadable(error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_grows_by_the_footer_only_once_it_is_shown() {
        let mut heights = Heights::default();
        assert_eq!(heights.fit(40), (40, 40));
        // The same need again changes nothing.
        assert_eq!(heights.fit(40), (0, 0));
        heights.footer = footer::HEIGHT;
        assert_eq!(heights.fit(40), (0, footer::HEIGHT));
        // Giving the room back shrinks the pages and the window alike; the footer stays.
        assert_eq!(heights.fit(0), (-40, -40));
        assert_eq!(heights.window, footer::HEIGHT);
    }
}
