//! The names saved clips get, `hindsight_YYYY-MM-DD_HH-MM-SS[-n].mp4` or `.mkv`, where `-n`
//! counts a second clip within one second. Screenshots are named the same way with their own
//! extension.

use crate::log;

const PREFIX: &str = "hindsight_";
/// `YYYY-MM-DD_HH-MM-SS`, as `log::file_stamp` writes it.
const STAMP_LENGTH: usize = 19;

/// The local time a clip was saved and its attempt within that second. Names order by both.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ClipName {
    stamp: String,
    attempt: u32,
}

impl ClipName {
    /// The first name for a clip saved now.
    pub(crate) fn now() -> Self {
        Self {
            stamp: log::file_stamp(),
            attempt: 1,
        }
    }

    /// The name for another clip saved within the same second.
    pub(crate) fn next(&self) -> Self {
        Self {
            stamp: self.stamp.clone(),
            attempt: self.attempt.saturating_add(1),
        }
    }

    pub(crate) fn file_name(&self, extension: &str) -> String {
        let Self { stamp, attempt } = self;
        if *attempt == 1 {
            format!("{PREFIX}{stamp}.{extension}")
        } else {
            format!("{PREFIX}{stamp}-{attempt}.{extension}")
        }
    }

    /// Parses a name that `file_name` produced with an extension `admits` accepts, returning
    /// `None` for any other file.
    pub(crate) fn parse(file_name: &str, admits: impl FnOnce(&str) -> bool) -> Option<Self> {
        let (rest, extension) = file_name.strip_prefix(PREFIX)?.rsplit_once('.')?;
        if !admits(extension) {
            return None;
        }
        let (stamp, attempt) = rest.split_at_checked(STAMP_LENGTH)?;
        if !is_stamp(stamp) {
            return None;
        }
        let attempt = if attempt.is_empty() {
            1
        } else {
            attempt.strip_prefix('-')?.parse().ok()?
        };
        Some(Self {
            stamp: stamp.to_owned(),
            attempt,
        })
    }

    /// The time saved, as `YYYY-MM-DD HH:MM:SS`.
    pub(crate) fn title(&self) -> String {
        let (date, time) = self.stamp.split_at(10);
        format!("{date} {}", time[1..].replace('-', ":"))
    }
}

fn is_stamp(stamp: &str) -> bool {
    stamp.len() == STAMP_LENGTH
        && stamp.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => byte == b'-',
            10 => byte == b'_',
            _ => byte.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Container;

    fn parse_clip(file_name: &str) -> Option<ClipName> {
        ClipName::parse(file_name, |extension| {
            Container::of_extension(extension).is_some()
        })
    }

    #[test]
    fn a_clips_name_gives_the_time_it_was_taken_and_its_attempt() {
        let name = parse_clip("hindsight_2026-10-12_21-14-05.mp4").unwrap();
        assert_eq!(name.title(), "2026-10-12 21:14:05");
        assert_eq!(name.attempt, 1);
        let again = parse_clip("hindsight_2026-10-12_21-14-05-2.mp4").unwrap();
        assert_eq!(again.stamp, name.stamp);
        assert_eq!(again.attempt, 2);
        for other in [
            "hindsight_2026-10-12_21-14-05.mp4.partial",
            "hindsight_2026-10-12_21-14.mp4",
            "hindsight_2026-10-12_21-14-05-x.mp4",
            "hindsight_2026-10-12-21-14-05.mp4",
            "boss fight.mp4",
            "hindsight_2026-10-12_21-14-05.png",
        ] {
            assert_eq!(parse_clip(other), None, "{other}");
        }
        assert_eq!(parse_clip("hindsight_2026-10-12_21-14-05.mkv"), Some(name));
    }

    #[test]
    fn a_saved_name_parses_back_to_itself() {
        let first = ClipName::now();
        let second = first.next();
        for name in [&first, &second, &second.next()] {
            assert_eq!(
                parse_clip(&name.file_name(Container::Mp4.extension())).as_ref(),
                Some(name)
            );
        }
        assert!(first < second);
        assert_eq!(
            first.file_name(Container::Mp4.extension()),
            format!("hindsight_{}.mp4", first.stamp)
        );
        assert_eq!(
            second.file_name(Container::Mp4.extension()),
            format!("hindsight_{}-2.mp4", first.stamp)
        );
        assert_eq!(
            second.file_name("png"),
            format!("hindsight_{}-2.png", first.stamp)
        );
        assert_eq!(parse_clip(&first.file_name("png")), None);
        assert_eq!(
            ClipName::parse(&second.file_name("png"), |extension| extension == "png").as_ref(),
            Some(&second)
        );
    }
}
