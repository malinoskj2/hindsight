//! The notification-area icon and its menu. Its callbacks run on the window's own thread
//! (the library delivers them from a hidden window's message handler), so they hand the
//! action straight to the window through a thread-local callback.

use crate::APP_NAME;
use std::{cell::RefCell, rc::Rc};
use thiserror::Error;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};

#[derive(Debug, Error)]
pub(crate) enum TrayError {
    #[error("system tray: {0}")]
    Icon(#[from] tray_icon::Error),
    #[error("tray menu: {0}")]
    Menu(#[from] tray_icon::menu::Error),
    #[error("tray icon image: {0}")]
    Image(#[from] tray_icon::BadIcon),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayAction {
    Show,
    Save,
    Quit,
}

type Handler = Rc<dyn Fn(TrayAction)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

pub(crate) struct Tray {
    _icon: TrayIcon,
}

impl Tray {
    pub(crate) fn new(on_action: impl Fn(TrayAction) + 'static) -> Result<Self, TrayError> {
        HANDLER.with(|handler| *handler.borrow_mut() = Some(Rc::new(on_action)));

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

        let actions = [
            (show.id().clone(), TrayAction::Show),
            (save.id().clone(), TrayAction::Save),
            (quit.id().clone(), TrayAction::Quit),
        ];
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some(&(_, action)) = actions.iter().find(|(id, _)| *id == event.id) {
                dispatch(action);
            }
        }));
        TrayIconEvent::set_event_handler(Some(|event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                dispatch(TrayAction::Show);
            }
        }));

        Ok(Self { _icon: icon })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        HANDLER.with(|handler| handler.borrow_mut().take());
    }
}

/// The callback is cloned out first, so it may itself replace or drop the tray.
fn dispatch(action: TrayAction) {
    let handler = HANDLER.with(|handler| handler.borrow().clone());
    if let Some(handler) = handler {
        handler(action);
    }
}

/// The application icon at the notification area's size.
fn app_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 32;
    Icon::from_rgba(super::icon::rgba(SIZE), SIZE, SIZE)
}
