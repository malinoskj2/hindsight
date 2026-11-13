use crate::config::DeviceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Picked {
    Default,
    Chosen,
    StandIn,
    /// The default output device, every app's sound, in place of one app's.
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows capture falls back")
    )]
    Everything,
}

pub(super) fn pick<D, E>(
    choice: Option<&DeviceId>,
    find: impl FnOnce(&DeviceId) -> Option<D>,
    default: impl FnOnce() -> Result<D, E>,
) -> Result<(D, Picked), E> {
    let Some(choice) = choice else {
        return default().map(|device| (device, Picked::Default));
    };
    match find(choice) {
        Some(device) => Ok((device, Picked::Chosen)),
        None => default().map(|device| (device, Picked::StandIn)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) id: DeviceId,
    pub(crate) name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Default,
    /// The sound of the full-screen app alone, whichever device it plays on.
    App,
    Present(Endpoint),
    /// The device chosen, which is not connected. It stays listed so that applying the settings
    /// keeps it.
    Absent {
        id: DeviceId,
        name: Option<String>,
    },
}

impl Entry {
    pub(crate) const fn device(&self) -> Option<&DeviceId> {
        match self {
            Self::Default | Self::App => None,
            Self::Present(endpoint) => Some(&endpoint.id),
            Self::Absent { id, .. } => Some(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selected<'a> {
    Default,
    App,
    Device(&'a DeviceId),
}

/// The entries a device list offers, and the position of the one selected. `offer_app` puts the
/// app's own sound second, after the default.
pub(crate) fn entries(
    mut present: Vec<Endpoint>,
    offer_app: bool,
    selected: Selected<'_>,
    name_of: impl FnOnce(&DeviceId) -> Option<String>,
) -> (Vec<Entry>, usize) {
    present.sort_by(|one, other| one.name.cmp(&other.name));
    let mut entries: Vec<Entry> = [Entry::Default]
        .into_iter()
        .chain(offer_app.then_some(Entry::App))
        .chain(present.into_iter().map(Entry::Present))
        .collect();
    let selected = match selected {
        Selected::App if offer_app => return (entries, 1),
        Selected::Default | Selected::App => return (entries, 0),
        Selected::Device(device) => device,
    };
    if let Some(position) = entries
        .iter()
        .position(|entry| entry.device() == Some(selected))
    {
        return (entries, position);
    }
    entries.push(Entry::Absent {
        id: selected.clone(),
        name: name_of(selected),
    });
    let last = entries.len() - 1;
    (entries, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> DeviceId {
        DeviceId::new(text.to_owned()).unwrap()
    }

    fn endpoint(text: &str, name: &str) -> Endpoint {
        Endpoint {
            id: id(text),
            name: name.to_owned(),
        }
    }

    #[test]
    fn the_default_stands_in_for_a_chosen_device_that_is_missing() {
        let present = |wanted: &DeviceId| (wanted.as_str() == "headset").then_some("headset");
        let default = || Ok::<_, ()>("speakers");

        assert_eq!(
            pick(None, present, default),
            Ok(("speakers", Picked::Default))
        );
        assert_eq!(
            pick(Some(&id("headset")), present, default),
            Ok(("headset", Picked::Chosen))
        );
        assert_eq!(
            pick(Some(&id("unplugged")), present, default),
            Ok(("speakers", Picked::StandIn))
        );
        assert_eq!(
            pick(Some(&id("unplugged")), present, || Err::<&str, _>("none")),
            Err("none")
        );
        assert_eq!(
            pick(Some(&id("headset")), present, || Err::<&str, _>("none")),
            Ok(("headset", Picked::Chosen)),
            "a chosen device needs no default"
        );
    }

    #[test]
    fn lists_start_with_the_default_and_keep_a_chosen_device_that_is_missing() {
        let present = vec![endpoint("b", "Speakers"), endpoint("a", "Headset")];

        let (entries, selected) = entries(present.clone(), false, Selected::Default, |_| None);
        assert_eq!(
            entries,
            [
                Entry::Default,
                Entry::Present(endpoint("a", "Headset")),
                Entry::Present(endpoint("b", "Speakers")),
            ]
        );
        assert_eq!(selected, 0);

        let (_, selected) =
            super::entries(present.clone(), false, Selected::Device(&id("b")), |_| {
                panic!("a device present is named by the list")
            });
        assert_eq!(selected, 2);

        let (entries, selected) =
            super::entries(present, false, Selected::Device(&id("gone")), |_| {
                Some("USB headset".to_owned())
            });
        assert_eq!(selected, 3);
        assert_eq!(
            entries[3],
            Entry::Absent {
                id: id("gone"),
                name: Some("USB headset".to_owned())
            }
        );
        assert_eq!(entries[3].device(), Some(&id("gone")));
        assert_eq!(entries[0].device(), None);
    }

    #[test]
    fn the_app_s_own_sound_is_offered_after_the_default() {
        let present = vec![endpoint("a", "Headset")];
        let (entries, selected) = super::entries(present.clone(), true, Selected::App, |_| None);
        assert_eq!(
            entries,
            [
                Entry::Default,
                Entry::App,
                Entry::Present(endpoint("a", "Headset"))
            ]
        );
        assert_eq!(selected, 1);
        assert_eq!(entries[1].device(), None);

        let (_, selected) =
            super::entries(present.clone(), true, Selected::Device(&id("a")), |_| None);
        assert_eq!(selected, 2);

        let (entries, selected) = super::entries(present, false, Selected::App, |_| None);
        assert_eq!(
            (entries.len(), selected),
            (2, 0),
            "not offered, so the default"
        );
    }
}
