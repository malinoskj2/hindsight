//! Modal loops: dialogs, message boxes and menus that dispatch the window's messages until they
//! end, the window's close among them.
//!
//! Destroying the window frees winsafe's handlers, so it waits while a handler is inside one.

use std::cell::Cell;

thread_local! {
    static OPEN: Cell<u32> = const { Cell::new(0) };
}

/// Runs `modal`, which dispatches messages, from a winsafe handler.
pub(super) fn run<T>(modal: impl FnOnce() -> T) -> T {
    let _open = Open::begin();
    modal()
}

/// Whether a handler is inside a modal loop, so that destroying the window would free it.
pub(super) fn is_open() -> bool {
    OPEN.with(|open| open.get() > 0)
}

struct Open;

impl Open {
    fn begin() -> Self {
        OPEN.with(|open| open.set(open.get() + 1));
        Self
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        OPEN.with(|open| open.set(open.get() - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modal_loop_is_open_until_every_nested_one_ends() {
        assert!(!is_open());
        run(|| {
            assert!(is_open());
            run(|| assert!(is_open()));
            assert!(is_open());
        });
        assert!(!is_open());
    }
}
