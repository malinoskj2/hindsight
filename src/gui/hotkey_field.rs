//! Hotkey fields, which enter a key combination pressed in them as the hotkey's text.

use super::layout;
use crate::sys::hotkey::{Hotkey, Modifiers};
use std::{cell::Cell, rc::Rc};
use winsafe::{self as w, co, gui, msg, prelude::*};

/// Whether a hotkey field has the keyboard. The registered hotkeys are set aside while it does,
/// because the system takes them before the field sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Capture {
    Began,
    Ended,
}

/// Makes `field` enter the hotkey for a combination pressed in it, such as Ctrl+Alt+F10, in the
/// text the hotkey parser reads. Presses that are not a hotkey, such as letters with or without
/// Shift, Backspace, Delete and Tab, type, edit and move the focus as in any other field, so a
/// hotkey can still be typed. Must be called before the field is created.
pub(super) fn capture(field: &gui::Edit) {
    // Whether the last key pressed was entered, so the character it makes is not typed as well.
    // The character is posted straight after its key, so no other key comes between them.
    let entered = Rc::new(Cell::new(false));
    for message in [co::WM::KEYDOWN, co::WM::SYSKEYDOWN] {
        let target = field.clone();
        let entered = Rc::clone(&entered);
        field.on_subclass().wm(message, move |params| {
            let hotkey = pressed(params.wparam);
            entered.set(hotkey.is_some());
            Ok(match hotkey {
                Some(hotkey) => {
                    enter(&target, hotkey);
                    0
                }
                None => pass_on(target.hwnd(), params),
            })
        });
    }
    for message in [co::WM::CHAR, co::WM::SYSCHAR] {
        let target = field.clone();
        let entered = Rc::clone(&entered);
        field.on_subclass().wm(message, move |params| {
            Ok(if entered.get() {
                0
            } else {
                pass_on(target.hwnd(), params)
            })
        });
    }
    // Print Screen reaches windows only as it is released.
    let target = field.clone();
    field.on_subclass().wm(co::WM::KEYUP, move |params| {
        Ok(match print_screen(params.wparam) {
            Some(hotkey) => {
                enter(&target, hotkey);
                0
            }
            None => pass_on(target.hwnd(), params),
        })
    });
    // Never passed on: released after a hotkey, Alt or F10 would open the window menu, which then
    // takes the next key.
    let target = field.clone();
    field.on_subclass().wm(co::WM::SYSKEYUP, move |params| {
        if let Some(hotkey) = print_screen(params.wparam) {
            enter(&target, hotkey);
        }
        Ok(0)
    });
}

fn pressed(key: usize) -> Option<Hotkey> {
    Hotkey::pressed(held_modifiers(), u16::try_from(key).ok()?)
}

fn print_screen(key: usize) -> Option<Hotkey> {
    if key == usize::from(co::VK::SNAPSHOT.raw()) {
        pressed(key)
    } else {
        None
    }
}

fn held_modifiers() -> Modifiers {
    [
        (co::VK::CONTROL, Modifiers::CTRL),
        (co::VK::MENU, Modifiers::ALT),
        (co::VK::SHIFT, Modifiers::SHIFT),
        (co::VK::LWIN, Modifiers::WIN),
        (co::VK::RWIN, Modifiers::WIN),
    ]
    .into_iter()
    .filter(|(key, _)| w::GetKeyState(*key).0)
    .fold(Modifiers::NONE, |held, (_, modifier)| held | modifier)
}

/// Selects the whole hotkey, so the next key typed replaces it and Backspace clears it.
fn enter(field: &gui::Edit, hotkey: Hotkey) {
    layout::set_text(field, &hotkey.to_string());
    field.set_selection(0, -1);
}

#[expect(unsafe_code, reason = "calls DefSubclassProc")]
fn pass_on(field: &w::HWND, message: msg::Wm) -> isize {
    // SAFETY: forwards the message the field received, unchanged, to its own window procedure.
    unsafe { field.DefSubclassProc(message) }
}
