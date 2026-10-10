//! What the app did and what went wrong: kept in memory for the Logs tab and mirrored to a
//! file in the state directory, so a problem can be read back or attached to a bug report.

use std::{
    collections::VecDeque,
    fmt::Display,
    fs::File,
    io::Write,
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
};

/// Entries kept in memory; the file keeps everything from the session.
const CAPACITY: usize = 500;
pub(crate) const FILE_NAME: &str = "hindsight.log";

/// How serious an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    /// The level as a line shows it.
    fn tag(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One recorded message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Increases by one per entry; lets a reader fetch only what it has not seen.
    pub(crate) sequence: u64,
    /// Wall-clock time, `HH:MM:SS`: local time on Windows, UTC elsewhere.
    pub(crate) time: String,
    pub(crate) level: Level,
    pub(crate) message: String,
}

impl Entry {
    /// One line: time, level and message.
    pub(crate) fn line(&self) -> String {
        format!("{}  {:<7}  {}", self.time, self.level.tag(), self.message)
    }
}

/// The entries still in memory, and the sequence number the next one gets.
struct Journal {
    entries: VecDeque<Entry>,
    next_sequence: u64,
}

impl Journal {
    /// Adds an entry under the next sequence number, forgetting the oldest one past
    /// [`CAPACITY`], and returns the entry's line.
    fn append(&mut self, time: String, level: Level, message: String) -> String {
        let entry = Entry {
            sequence: self.next_sequence,
            time,
            level,
            message,
        };
        let line = entry.line();
        self.next_sequence += 1;
        self.entries.push_back(entry);
        if self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
        line
    }
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    entries: VecDeque::new(),
    next_sequence: 1,
});

/// The mirror file, apart from the journal so that readers never wait on the disk. A writer
/// takes this lock first and the journal lock inside it, which keeps the file in sequence order.
static MIRROR: Mutex<Option<File>> = Mutex::new(None);

/// Records `message` as information.
pub(crate) fn info(message: impl Display) {
    record(Level::Info, message);
}

/// Records `message` as a warning.
pub(crate) fn warning(message: impl Display) {
    record(Level::Warning, message);
}

/// Records `message` as an error.
pub(crate) fn error(message: impl Display) {
    record(Level::Error, message);
}

/// Entries recorded after `sequence` (0 for all that are still in memory), oldest first.
pub(crate) fn since(sequence: u64) -> Vec<Entry> {
    journal()
        .entries
        .iter()
        .filter(|entry| entry.sequence > sequence)
        .cloned()
        .collect()
}

/// Writes the entries so far to `path`, replacing the file from an earlier run, and every
/// later entry as it is recorded.
pub(crate) fn mirror_to(path: &Path) -> std::io::Result<()> {
    let mut created = File::create(path)?;
    let mut file = mirror();
    // No entry is recorded while the mirror lock is held, so nothing falls between the copy and
    // the switch to the new file.
    let lines: Vec<String> = journal().entries.iter().map(Entry::line).collect();
    for line in lines {
        writeln!(created, "{line}")?;
    }
    *file = Some(created);
    Ok(())
}

/// Adds an entry to the journal and, when mirroring, its line to the file.
fn record(level: Level, message: impl Display) {
    let time = timestamp();
    let message = message.to_string();
    let mut file = mirror();
    let line = journal().append(time, level, message);
    if let Some(file) = file.as_mut() {
        // A full disk must not take the application down with it.
        let _ = writeln!(file, "{line}");
    }
}

fn journal() -> MutexGuard<'static, Journal> {
    JOURNAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn mirror() -> MutexGuard<'static, Option<File>> {
    MIRROR.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops mirroring and hands back the file, so a test can let its directory go.
#[cfg(test)]
fn detach_mirror() -> Option<File> {
    mirror().take()
}

/// The local time for a log entry, `HH:MM:SS`.
#[cfg(windows)]
fn timestamp() -> String {
    let now = winsafe::GetLocalTime();
    format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond)
}

/// The local date and time for a file name, `YYYY-MM-DD_HH-MM-SS`.
#[cfg(windows)]
pub(crate) fn file_stamp() -> String {
    let now = winsafe::GetLocalTime();
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

/// The local date, `YYYY-MM-DD`.
#[cfg(windows)]
pub(crate) fn today() -> String {
    let now = winsafe::GetLocalTime();
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

/// The UTC time for a log entry, `HH:MM:SS`.
#[cfg(not(windows))]
fn timestamp() -> String {
    let time = unix_seconds() % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

/// The UTC date and time for a file name, `YYYY-MM-DD_HH-MM-SS`.
#[cfg(not(windows))]
pub(crate) fn file_stamp() -> String {
    let seconds = unix_seconds();
    let (year, month, day) = civil_date(seconds / 86_400);
    let time = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}_{:02}-{:02}-{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

/// Seconds since 1970-01-01 UTC, or 0 if the clock is set before then.
#[cfg(not(windows))]
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
#[cfg(not(windows))]
fn civil_date(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The journal is process-wide, so every test reasons relative to its own entries.

    #[test]
    fn entries_are_sequenced_and_formatted_as_lines() {
        info("log test: first");
        let first = since(0)
            .into_iter()
            .rev()
            .find(|entry| entry.message == "log test: first")
            .unwrap();
        assert_eq!(first.level, Level::Info);
        assert_eq!(first.time.len(), 8);
        assert_eq!(&first.line()[8..], "  info     log test: first");

        error("log test: second");
        let later = since(first.sequence);
        assert!(later.iter().all(|entry| entry.sequence > first.sequence));
        let second = later
            .iter()
            .find(|entry| entry.message == "log test: second")
            .unwrap();
        assert_eq!(second.level, Level::Error);
        assert_eq!(&second.line()[8..], "  error    log test: second");
        assert!(
            since(second.sequence)
                .iter()
                .all(|entry| entry.sequence > second.sequence)
        );
    }

    #[test]
    fn memory_keeps_only_the_most_recent_entries() {
        for index in 0..CAPACITY + 10 {
            warning(format!("log test: fill {index}"));
        }
        let kept = since(0);
        assert!(kept.len() <= CAPACITY);
        assert!(
            kept.iter()
                .any(|entry| entry.message == format!("log test: fill {}", CAPACITY + 9))
        );
        assert!(!kept.iter().any(|entry| entry.message == "log test: fill 0"));
    }

    #[test]
    fn file_stamps_sort_by_time_and_are_valid_file_names() {
        let stamp = file_stamp();
        assert_eq!(stamp.len(), 19);
        assert_eq!(&stamp[10..11], "_");
        assert!(
            stamp
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, '-' | '_'))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn civil_dates_cover_leap_years_and_century_rules() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_736), (2026, 10, 10));
        assert_eq!(civil_date(47_540), (2100, 2, 28));
        assert_eq!(civil_date(47_541), (2100, 3, 1));
    }

    #[test]
    fn mirrors_earlier_and_later_entries_to_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        info("log test: before mirror");
        mirror_to(&path).unwrap();
        info("log test: after mirror");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("info     log test: before mirror\n"));
        assert!(written.contains("info     log test: after mirror\n"));

        // Windows cannot delete the directory while the journal still holds the file open.
        assert!(detach_mirror().is_some());
        directory.close().unwrap();
    }
}
