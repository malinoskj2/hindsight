//! The running version's release notes, which the build embeds from `CHANGELOG.md`, and when a
//! start shows them.

use super::Version;
use std::cmp::Ordering;

/// Empty when a development build's changelog has no section for its version. A release build
/// fails instead.
const EMBEDDED: &str = include_str!(concat!(env!("OUT_DIR"), "/release-notes.md"));
const BULLET: &str = "- ";
const SHOWN_BULLET: &str = "• ";

/// The running version's notes, in the changelog's Markdown.
pub(crate) fn running() -> Option<&'static str> {
    (!EMBEDDED.is_empty()).then_some(EMBEDDED)
}

/// The notes as plain text for a multi-line edit control, which needs CRLF line breaks and draws
/// no Markdown, so list items get a bullet character instead of their dash.
pub(crate) fn displayed(notes: &str) -> String {
    notes
        .lines()
        .map(|line| match line.strip_prefix(BULLET) {
            Some(item) => format!("{SHOWN_BULLET}{item}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// What a start does about the running version's notes, given the version that ran before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// This version has run here before.
    Again,
    /// A first run, or a downgrade, whose notes would describe nothing new. The version is
    /// recorded straight away.
    Unannounced,
    /// An upgrade. The notes are shown once the window is, and the version recorded after.
    Upgraded,
}

impl Arrival {
    pub(crate) fn of(last: Option<&Version>, running: &Version) -> Self {
        match last.map(|last| last.cmp(running)) {
            None | Some(Ordering::Greater) => Self::Unannounced,
            Some(Ordering::Equal) => Self::Again,
            Some(Ordering::Less) => Self::Upgraded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::changelog, *};

    fn version(text: &str) -> Version {
        text.parse().unwrap()
    }

    #[test]
    fn the_embedded_notes_are_the_changelogs_section_for_this_version() {
        assert_eq!(
            running(),
            changelog::section(
                include_str!("../../CHANGELOG.md"),
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn notes_show_as_plain_lines_with_bullets() {
        assert_eq!(
            displayed("Changes:\r\n\r\n- Saves faster.\n- Uses less memory.\n  - nested"),
            "Changes:\r\n\r\n• Saves faster.\r\n• Uses less memory.\r\n  - nested"
        );
    }

    #[test]
    fn only_an_upgrade_shows_the_notes() {
        let running = version("0.2.1");
        assert_eq!(
            Arrival::of(Some(&version("0.2.0")), &running),
            Arrival::Upgraded
        );
        assert_eq!(
            Arrival::of(Some(&version("0.2.1-beta.1")), &running),
            Arrival::Upgraded
        );
        assert_eq!(
            Arrival::of(Some(&version("0.2.1")), &running),
            Arrival::Again
        );
        assert_eq!(
            Arrival::of(Some(&version("0.3.0")), &running),
            Arrival::Unannounced
        );
        assert_eq!(Arrival::of(None, &running), Arrival::Unannounced);
    }
}
