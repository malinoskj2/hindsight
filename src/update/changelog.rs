//! Reads a version's notes from `CHANGELOG.md`.

const HEADING: &str = "## ";
const NEXT_HEADING: &str = "\n## ";

pub(crate) fn section<'a>(changelog: &'a str, version: &str) -> Option<&'a str> {
    changelog
        .split(NEXT_HEADING)
        .enumerate()
        .filter_map(|(index, section)| {
            if index == 0 {
                section.strip_prefix(HEADING)
            } else {
                Some(section)
            }
        })
        .find_map(|section| {
            let (title, notes) = section.split_once('\n').unwrap_or((section, ""));
            (title.split_whitespace().next() == Some(version)).then(|| notes.trim())
        })
        .filter(|notes| !notes.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "# Changelog

## 0.3.0

- Newest.

## 0.2.1

- Fixed one thing.
- Fixed another.

### Known issues

- One.

## 0.2.0

- First.
";

    #[test]
    fn a_version_finds_its_own_section() {
        assert_eq!(
            section(CHANGELOG, "0.2.1"),
            Some("- Fixed one thing.\n- Fixed another.\n\n### Known issues\n\n- One.")
        );
        assert_eq!(section(CHANGELOG, "0.3.0"), Some("- Newest."));
    }

    #[test]
    fn the_last_section_runs_to_the_end() {
        assert_eq!(section(CHANGELOG, "0.2.0"), Some("- First."));
        assert_eq!(section("## 0.1.0\n- Only.", "0.1.0"), Some("- Only."));
    }

    #[test]
    fn a_missing_or_empty_section_is_none() {
        assert_eq!(section(CHANGELOG, "0.2.2"), None);
        assert_eq!(section(CHANGELOG, "0.2"), None);
        assert_eq!(section(CHANGELOG, "0.2.1-beta.1"), None);
        assert_eq!(section("", "0.2.0"), None);
        assert_eq!(section("## 0.2.0\n\n## 0.1.0\n- Old.", "0.2.0"), None);
        assert_eq!(section("## 0.2.0", "0.2.0"), None);
    }

    #[test]
    fn windows_line_endings_and_a_dated_heading_are_read() {
        let changelog = "# Changelog\r\n\r\n## 0.2.0 (2026-10-27)\r\n\r\n- First.\r\n";
        assert_eq!(section(changelog, "0.2.0"), Some("- First."));
    }
}
