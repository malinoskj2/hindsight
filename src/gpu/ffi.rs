//! What the bindings to the NVIDIA driver's interfaces have in common.

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

impl Library {
    pub(super) fn load(name: PCWSTR) -> windows::core::Result<Self> {
        // SAFETY: loads a system library by name.
        let module = unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
        // SAFETY: the module was just loaded, and only this `Owned` frees it.
        Ok(Self(unsafe { Owned::new(module) }))
    }

    /// # Safety
    /// `F` must be the signature the library exports `name` with.
    pub(super) unsafe fn export<F: Copy>(&self, name: PCSTR) -> Option<F> {
        const {
            assert!(size_of::<F>() == size_of::<usize>());
        }
        // SAFETY: looks a symbol up in the loaded library.
        let address = unsafe { GetProcAddress(*self.0, name) }?;
        // SAFETY: a function pointer of the size of `F`, with the signature the caller vouches for.
        Some(unsafe {
            std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, F>(&address)
        })
    }
}

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
