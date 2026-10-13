//! The one handler a Win32 facility reports to, kept on the thread its callbacks arrive on.

use std::{cell::RefCell, rc::Rc};

pub(crate) struct Slot<F: ?Sized>(RefCell<Option<Rc<F>>>);

impl<F: ?Sized> Slot<F> {
    pub(crate) const fn new() -> Self {
        Self(RefCell::new(None))
    }

    pub(crate) fn set(&self, handler: Rc<F>) {
        *self.0.borrow_mut() = Some(handler);
    }

    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }

    /// Cloned out, so the handler may itself replace or clear the slot.
    pub(crate) fn get(&self) -> Option<Rc<F>> {
        self.0.borrow().clone()
    }
}
