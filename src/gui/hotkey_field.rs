//! Hotkey fields, which take the key combination pressed in them as the hotkey. Nothing is typed
//! into them.

use super::{layout, text};
use crate::sys::{
    callback::Slot,
    hotkey::{self, Hotkey, Modifiers, Press, RegisterError},
};
use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
};
use winsafe::{self as w, co, gui, msg, prelude::*};

/// Whether a hotkey field has the keyboard. The registered hotkeys are set aside while it does,
/// because the system takes them before the field sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Capture {
    Began,
    Ended,
}

/// Why the keys last pressed in a field were not taken as its hotkey.
pub(super) enum Refusal {
    Unavailable(RegisterError),
    NeedsModifier(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(RegisterError::Taken(hotkey)) => {
                formatter.write_str(&text::hotkey_taken(hotkey))
            }
            Self::Unavailable(RegisterError::System(hotkey, error)) => {
                formatter.write_str(&text::hotkey_refused(hotkey, error))
            }
            Self::NeedsModifier(key) => formatter.write_str(&text::hotkey_needs_modifier(key)),
        }
    }
}

type Refused = dyn Fn(Option<&Refusal>);

#[derive(Clone)]
pub(super) struct HotkeyField(Rc<Inner>);

struct Inner {
    edit: gui::Edit,
    /// The hotkey shown when the field took the keyboard, shown again if it is left with none.
    kept: RefCell<String>,
    /// What the field shows while no modifier is held: empty, for the prompt, until a hotkey is
    /// taken.
    resting: RefCell<String>,
    /// Keys seen going down since the field took the keyboard. A key released without going down
    /// here was taken by another app's hotkey, or is Print Screen, which Windows keeps from
    /// windows until it is released.
    down: RefCell<Vec<u16>>,
    /// Whether the field shows the modifiers held, which a key completes.
    composing: Cell<bool>,
    refused: Slot<Refused>,
}

impl HotkeyField {
    /// Must be called before `edit` is created.
    pub(super) fn new(edit: gui::Edit) -> Self {
        let field = Self(Rc::new(Inner {
            edit,
            kept: RefCell::default(),
            resting: RefCell::default(),
            down: RefCell::default(),
            composing: Cell::new(false),
            refused: Slot::new(),
        }));
        field.events();
        field
    }

    pub(super) fn edit(&self) -> &gui::Edit {
        &self.0.edit
    }

    /// `refused` is called with each refusal, and with `None` once a hotkey is taken or Escape
    /// keeps the one there was.
    pub(super) fn on_refused(&self, refused: impl Fn(Option<&Refusal>) + 'static) {
        self.0.refused.set(Rc::new(refused));
    }

    fn events(&self) {
        let events = self.0.edit.on_subclass();
        let field = self.clone();
        events.wm(co::WM::SETFOCUS, move |params| {
            field.begin();
            let result = field.pass_on(params);
            // The field takes no typing, so a caret would only suggest it did.
            let _ = field.0.edit.hwnd().HideCaret();
            Ok(result)
        });
        let field = self.clone();
        events.wm(co::WM::KILLFOCUS, move |params| {
            field.end();
            Ok(field.pass_on(params))
        });
        let field = self.clone();
        events.wm(co::WM::GETDLGCODE, move |params| {
            let wanted = if wants_escape(&params) {
                usize::from(co::DLGC::WANTMESSAGE.raw()).cast_signed()
            } else {
                0
            };
            Ok(field.pass_on(params) | wanted)
        });
        for message in [co::WM::KEYDOWN, co::WM::SYSKEYDOWN] {
            let field = self.clone();
            events.wm(message, move |params| {
                if let Some(key) = key(params.wparam) {
                    field.key_down(key);
                }
                Ok(0)
            });
        }
        // Never passed on: released after a hotkey, Alt or F10 would open the window menu, which
        // then takes the next key.
        for message in [co::WM::KEYUP, co::WM::SYSKEYUP] {
            let field = self.clone();
            events.wm(message, move |params| {
                if let Some(key) = key(params.wparam) {
                    field.key_up(key);
                }
                Ok(0)
            });
        }
        for message in [co::WM::CHAR, co::WM::SYSCHAR, co::WM::CONTEXTMENU] {
            events.wm(message, |_| Ok(0));
        }
    }

    fn begin(&self) {
        let edit = &self.0.edit;
        *self.0.kept.borrow_mut() = edit.text().unwrap_or_default();
        self.0.resting.borrow_mut().clear();
        self.0.down.borrow_mut().clear();
        self.0.composing.set(false);
        show_prompt(edit);
        layout::set_text(edit, "");
    }

    fn end(&self) {
        let resting = self.0.resting.take();
        let shown = if resting.is_empty() {
            self.0.kept.take()
        } else {
            resting
        };
        self.0.composing.set(false);
        layout::set_text(&self.0.edit, &shown);
        self.0.edit.set_selection(0, 0);
    }

    fn key_down(&self, key: u16) {
        {
            let mut down = self.0.down.borrow_mut();
            if down.contains(&key) {
                return;
            }
            down.push(key);
        }
        if key == co::VK::ESCAPE.raw() {
            self.keep();
        } else if is_modifier(key) {
            self.0.composing.set(true);
            self.show_held();
        } else {
            self.press(key);
        }
    }

    fn key_up(&self, key: u16) {
        let was_down = {
            let mut down = self.0.down.borrow_mut();
            let position = down.iter().position(|&seen| seen == key);
            position
                .map(|position| down.swap_remove(position))
                .is_some()
        };
        if is_modifier(key) {
            if self.0.composing.get() {
                self.show_held();
            }
        } else if !was_down {
            self.press(key);
        }
    }

    /// Escape keeps the hotkey there was.
    fn keep(&self) {
        let kept = self.0.kept.borrow().clone();
        self.0.composing.set(false);
        layout::set_text(&self.0.edit, &kept);
        *self.0.resting.borrow_mut() = kept;
        self.refuse(None);
    }

    fn press(&self, key: u16) {
        let refusal = match Hotkey::pressed(held_modifiers(), key) {
            Press::Hotkey(hotkey) => match hotkey::available(self.0.edit.hwnd(), hotkey) {
                Ok(()) => {
                    self.take(hotkey);
                    None
                }
                Err(error) => Some(Refusal::Unavailable(error)),
            },
            Press::NeedsModifier(key) => Some(Refusal::NeedsModifier(key)),
            Press::Ignored => return,
        };
        self.0.composing.set(false);
        if refusal.is_some() {
            self.show_resting();
        }
        self.refuse(refusal.as_ref());
    }

    fn take(&self, hotkey: Hotkey) {
        let shown = hotkey.to_string();
        layout::set_text(&self.0.edit, &shown);
        self.0.edit.set_selection(0, -1);
        *self.0.resting.borrow_mut() = shown;
    }

    fn show_held(&self) {
        let held = held_modifiers();
        if held == Modifiers::NONE {
            self.0.composing.set(false);
            self.show_resting();
        } else {
            layout::set_text(&self.0.edit, &format!("{held}…"));
        }
    }

    fn show_resting(&self) {
        let resting = self.0.resting.borrow().clone();
        layout::set_text(&self.0.edit, &resting);
        self.0.edit.set_selection(0, -1);
    }

    fn refuse(&self, refusal: Option<&Refusal>) {
        if let Some(refused) = self.0.refused.get() {
            refused(refusal);
        }
    }

    #[expect(unsafe_code, reason = "calls DefSubclassProc")]
    fn pass_on(&self, message: msg::Wm) -> isize {
        // SAFETY: forwards the message the field received, unchanged, to its own window procedure.
        unsafe { self.0.edit.hwnd().DefSubclassProc(message) }
    }
}

/// The dialog manager takes Escape for the page unless the field asks for it.
#[expect(unsafe_code, reason = "reads WM_GETDLGCODE's message")]
fn wants_escape(params: &msg::Wm) -> bool {
    // SAFETY: the parameters are WM_GETDLGCODE's, whose message pointer, when there is one, is
    // valid for the duration of the call.
    let code = unsafe { msg::WmGetDlgCode::from_generic_wm(*params) };
    code.msg.is_some_and(|message| {
        message.message == co::WM::KEYDOWN && message.wParam == usize::from(co::VK::ESCAPE.raw())
    })
}

#[expect(unsafe_code, reason = "sends EM_SETCUEBANNER")]
fn show_prompt(edit: &gui::Edit) {
    // SAFETY: the message carries a string that outlives the call.
    let _ = unsafe {
        edit.hwnd().SendMessage(msg::EmSetCueBanner {
            show_even_with_focus: true,
            text: w::WString::from_str(text::HOTKEY_PROMPT),
        })
    };
}

fn key(wparam: usize) -> Option<u16> {
    u16::try_from(wparam).ok()
}

fn is_modifier(key: u16) -> bool {
    [
        co::VK::CONTROL,
        co::VK::MENU,
        co::VK::SHIFT,
        co::VK::LWIN,
        co::VK::RWIN,
    ]
    .iter()
    .any(|modifier| modifier.raw() == key)
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
