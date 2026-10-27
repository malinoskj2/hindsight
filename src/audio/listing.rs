//! The devices Settings offers, and whether they changed since Settings last listed them.

use super::{
    choice::Endpoint,
    wasapi::{Devices, Registration},
};
use crate::{config::DeviceId, replay::Source, sys::com::Apartment};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Belongs to the thread that opened it, whose COM apartment it holds. Fields drop in this order,
/// so the registration and the enumerator are released before COM is uninitialised.
pub(crate) struct DeviceList {
    _registration: Registration,
    devices: Devices,
    changed: Arc<AtomicBool>,
    _com: Apartment,
}

impl DeviceList {
    /// Opens the list in a single-threaded apartment, as the window's thread uses for the
    /// shell's dialogs.
    pub(crate) fn open() -> windows::core::Result<Self> {
        let com = Apartment::single_threaded()?;
        let devices = Devices::new()?;
        let changed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&changed);
        let registration = devices.register(move || flag.store(true, Ordering::Relaxed))?;
        Ok(Self {
            _registration: registration,
            devices,
            changed,
            _com: com,
        })
    }

    /// Returns the connected devices `source` can be captured from.
    pub(crate) fn endpoints(&self, source: Source) -> windows::core::Result<Vec<Endpoint>> {
        self.devices.endpoints(source)
    }

    /// Returns the name of the device with endpoint ID `id`, connected or not, if Windows knows
    /// it.
    pub(crate) fn name(&self, id: &DeviceId) -> Option<String> {
        self.devices.name(id)
    }

    /// Returns whether a device came or went, was enabled or disabled, or became a default since
    /// the last call.
    pub(crate) fn changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }
}
