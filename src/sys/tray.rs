//! The notification-area icon and its menu. Its callbacks run on the window's own thread
//! (the library delivers them from a hidden window's message handler), so they hand the
//! action straight to the window through a thread-local callback.

use crate::APP_NAME;
use std::{cell::RefCell, rc::Rc};
use thiserror::Error;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

/// The side of the icon in the notification area, in pixels.
const ICON_SIZE: u32 = 32;

/// The callback of the tray.
type Handler = Rc<dyn Fn(TrayAction)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

#[derive(Debug, Error)]
pub(crate) enum TrayError {
    #[error("system tray: {0}")]
    Icon(#[from] tray_icon::Error),
    #[error("tray menu: {0}")]
    Menu(#[from] tray_icon::menu::Error),
    #[error("tray icon image: {0}")]
    Image(#[from] tray_icon::BadIcon),
}

/// What the user asked for from the icon or its menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    Show,
    Save,
    Quit,
}

/// Calls its handler, on the creating thread, with each action chosen from the icon or its
/// menu. The icon is removed when dropped.
pub(crate) struct Tray {
    _icon: TrayIcon,
}

impl Tray {
    pub(crate) fn new(on_action: impl Fn(TrayAction) + 'static) -> Result<Self, TrayError> {
        let menu = Menu::new();
        let show = MenuItem::new(format!("Show {APP_NAME}"), true, None);
        let save = MenuItem::new("Save replay", true, None);
        let quit = MenuItem::new(format!("Quit {APP_NAME}"), true, None);
        menu.append_items(&[&show, &save, &quit])?;

        let icon = TrayIconBuilder::new()
            .with_icon(app_icon()?)
            .with_tooltip(format!("{APP_NAME} · click to open"))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        // Installed only once nothing can fail any more: a handler left behind by a failed
        // setup would keep the window, and through it the worker, alive for good.
        set_handler(Rc::new(on_action));
        route_events([
            (show.id().clone(), TrayAction::Show),
            (save.id().clone(), TrayAction::Save),
            (quit.id().clone(), TrayAction::Quit),
        ]);
        Ok(Self { _icon: icon })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        clear_handler();
    }
}

/// The application icon at the notification area's size.
fn app_icon() -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(super::icon::rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE)
}

/// Sends a choice from the menu, whose items' ids map to `actions`, and a click that opens the
/// window to the callback.
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

/// Whether a click on the icon shows the window: a left click once the button is released, or
/// a left double click.
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

/// Makes `handler` this thread's callback, replacing any other.
fn set_handler(handler: Handler) {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(handler));
}

/// Forgets this thread's callback.
fn clear_handler() {
    HANDLER.with(|slot| slot.borrow_mut().take());
}

/// Calls this thread's callback, if there is one. It is cloned out first, so it may itself
/// replace or drop the tray.
fn dispatch(action: TrayAction) {
    let handler = HANDLER.with(|slot| slot.borrow().clone());
    if let Some(handler) = handler {
        handler(action);
    }
}
