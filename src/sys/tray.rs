//! The notification-area icon and its menu; callbacks arrive on the window's own thread.

use super::{callback::Slot, icon};
use crate::{APP_NAME, config::Theme};
use std::rc::Rc;
use thiserror::Error;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

const ICON_SIZE: u32 = 32;

thread_local! {
    static HANDLER: Slot<dyn Fn(TrayAction)> = const { Slot::new() };
}

/// Why the tray icon failed; a message ends with its cause, which `source()` doesn't repeat.
#[derive(Debug, Error)]
pub(crate) enum TrayError {
    #[error("system tray: {0}")]
    Icon(tray_icon::Error),
    #[error("tray menu: {0}")]
    Menu(tray_icon::menu::Error),
    #[error("tray icon image: {0}")]
    Image(tray_icon::BadIcon),
}

impl From<tray_icon::Error> for TrayError {
    fn from(error: tray_icon::Error) -> Self {
        Self::Icon(error)
    }
}

impl From<tray_icon::menu::Error> for TrayError {
    fn from(error: tray_icon::menu::Error) -> Self {
        Self::Menu(error)
    }
}

impl From<tray_icon::BadIcon> for TrayError {
    fn from(error: tray_icon::BadIcon) -> Self {
        Self::Image(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    Show,
    Save,
    Quit,
}

pub(crate) struct Tray {
    icon: TrayIcon,
}

impl Tray {
    pub(crate) fn new(
        theme: Theme,
        on_action: impl Fn(TrayAction) + 'static,
    ) -> Result<Self, TrayError> {
        let menu = Menu::new();
        let show = MenuItem::new(format!("Show {APP_NAME}"), true, None);
        let save = MenuItem::new("Save replay", true, None);
        let quit = MenuItem::new(format!("Quit {APP_NAME}"), true, None);
        menu.append_items(&[&show, &save, &quit])?;

        let icon = TrayIconBuilder::new()
            .with_icon(app_icon(theme)?)
            .with_tooltip(format!("{APP_NAME} · click to open"))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        // Installed last: a handler left behind by a failed setup would keep the window alive.
        HANDLER.with(|slot| slot.set(Rc::new(on_action)));
        route_events([
            (show.id().clone(), TrayAction::Show),
            (save.id().clone(), TrayAction::Save),
            (quit.id().clone(), TrayAction::Quit),
        ]);
        Ok(Self { icon })
    }

    pub(crate) fn set_theme(&self, theme: Theme) -> Result<(), TrayError> {
        self.icon.set_icon(Some(app_icon(theme)?))?;
        Ok(())
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        HANDLER.with(Slot::clear);
    }
}

fn app_icon(theme: Theme) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_resource(icon::resource(theme), Some((ICON_SIZE, ICON_SIZE)))
}

fn route_events(actions: [(MenuId, TrayAction); 3]) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(&(_, action)) = actions.iter().find(|(id, _)| *id == event.id) {
            dispatch(action);
        }
    }));
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if opens_window(&event) {
            dispatch(TrayAction::Show);
        }
    }));
}

fn opens_window(event: &TrayIconEvent) -> bool {
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        }
    )
}

fn dispatch(action: TrayAction) {
    if let Some(handler) = HANDLER.with(Slot::get) {
        handler(action);
    }
}
