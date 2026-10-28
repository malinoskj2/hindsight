use crate::config::DeviceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Picked {
    Default,
    Chosen,
    StandIn,
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
            Self::Default => None,
            Self::Present(endpoint) => Some(&endpoint.id),
            Self::Absent { id, .. } => Some(id),
        }
    }
}

pub(crate) fn entries(
    mut present: Vec<Endpoint>,
    selected: Option<&DeviceId>,
    name_of: impl FnOnce(&DeviceId) -> Option<String>,
) -> (Vec<Entry>, usize) {
    present.sort_by(|one, other| one.name.cmp(&other.name));
    let mut entries: Vec<Entry> = [Entry::Default]
        .into_iter()
        .chain(present.into_iter().map(Entry::Present))
        .collect();
    let Some(selected) = selected else {
        return (entries, 0);
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

        let (entries, selected) = entries(present.clone(), None, |_| None);
        assert_eq!(
            entries,
            [
                Entry::Default,
                Entry::Present(endpoint("a", "Headset")),
                Entry::Present(endpoint("b", "Speakers")),
            ]
        );
        assert_eq!(selected, 0);

        let (_, selected) = super::entries(present.clone(), Some(&id("b")), |_| {
            panic!("a device present is named by the list")
        });
        assert_eq!(selected, 2);

        let (entries, selected) = super::entries(present, Some(&id("gone")), |_| {
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
}
