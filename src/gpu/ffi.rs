//! What the bindings to the NVIDIA driver's interfaces have in common.

use std::{marker::PhantomData, ops::Deref};
use windows::{
    Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    },
    core::{Owned, PCSTR, PCWSTR},
};

/// A library from System32 only, never a planted copy; everything looked up in or made by it
/// must be gone before it is dropped.
pub(super) struct Library(Owned<HMODULE>);

#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "a module handle is not tied to the thread that loaded it"
)]
// SAFETY: an HMODULE may be freed from any thread.
unsafe impl Send for Library {}
// SAFETY: a shared library only looks symbols up, which `GetProcAddress` does from any thread.
unsafe impl Sync for Library {}

impl Library {
    pub(super) fn load(name: PCWSTR) -> windows::core::Result<Self> {
        // SAFETY: `name` is a NUL-terminated wide string, and the search is limited to System32.
        let module = unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
        // SAFETY: the module was just loaded, and only this `Owned` frees it.
        Ok(Self(unsafe { Owned::new(module) }))
    }

    /// # Safety
    /// `F` must be the signature the library exports `name` with.
    pub(super) unsafe fn export<F: FnPtr>(&self, name: PCSTR) -> Option<Symbol<'_, F>> {
        const {
            assert!(size_of::<F>() == size_of::<usize>());
        }
        // SAFETY: the module is loaded while `self` lives; a missing name returns `None`.
        let address = unsafe { GetProcAddress(*self.0, name) }?;
        // SAFETY: both are function pointers of the same size, and the caller vouches that the
        // export has the signature `F`.
        let function = unsafe {
            std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, F>(&address)
        };
        Some(Symbol {
            function,
            _library: PhantomData,
        })
    }
}

/// A function exported by a [`Library`], usable while the library is borrowed.
///
/// Copying the pointer out with `*` detaches it from the library's lifetime: whoever does so
/// keeps the library loaded for as long as the copy is called, as a struct does by holding the
/// library in a field declared after the pointers.
pub(super) struct Symbol<'lib, F> {
    function: F,
    _library: PhantomData<&'lib Library>,
}

impl<F> Deref for Symbol<'_, F> {
    type Target = F;

    fn deref(&self) -> &F {
        &self.function
    }
}

mod sealed {
    pub(in crate::gpu) trait Sealed {}
}

/// An `unsafe extern "system"` function pointer: the only types a library export may be read as.
pub(super) trait FnPtr: Copy + sealed::Sealed {}

macro_rules! fn_ptr {
    ($($argument:ident),*) => {
        impl<R, $($argument),*> sealed::Sealed for unsafe extern "system" fn($($argument),*) -> R {}
        impl<R, $($argument),*> FnPtr for unsafe extern "system" fn($($argument),*) -> R {}
    };
}

fn_ptr!();
fn_ptr!(A);
fn_ptr!(A, B);
fn_ptr!(A, B, C);
fn_ptr!(A, B, C, D);
fn_ptr!(A, B, C, D, E);
fn_ptr!(A, B, C, D, E, G);

/// A status code kept raw, since a driver may return codes this program does not know, with
/// the names of the ones it does.
macro_rules! raw_status {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident, $unknown:literal {
            $($code:literal => $text:literal),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        $vis struct $name(i32);

        impl $name {
            pub(super) const SUCCESS: Self = Self(0);

            pub(super) fn ok(self) -> Result<(), Self> {
                if self == Self::SUCCESS { Ok(()) } else { Err(self) }
            }

            const fn name(self) -> &'static str {
                match self.0 {
                    $($code => $text,)*
                    _ => $unknown,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(formatter, "{} ({})", self.name(), self.0)
            }
        }
    };
}
pub(super) use raw_status;

/// All-zero is a valid value.
/// # Safety
/// Implementors contain only integers, GUIDs, raw pointers, optional function pointers and
/// unions or arrays of those.
pub(super) unsafe trait Zeroed: Sized {
    fn zeroed() -> Self {
        // SAFETY: guaranteed by the trait's contract.
        unsafe { std::mem::zeroed() }
    }
}
