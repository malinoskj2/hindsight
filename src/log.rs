//! In-memory log for the Logs tab, mirrored to a file in the state directory.

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) sequence: u64,
    /// `HH:MM:SS`, local time on Windows, UTC elsewhere.
    pub(crate) time: String,
    pub(crate) level: Level,
    pub(crate) message: String,
}

impl Entry {
    pub(crate) fn line(&self) -> String {
        format!("{}  {:<7}  {}", self.time, self.level.tag(), self.message)
    }
}

struct Journal {
    entries: VecDeque<Entry>,
    next_sequence: u64,
}

impl Journal {
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

/// Locked before the journal, which keeps the file in sequence order.
static MIRROR: Mutex<Option<File>> = Mutex::new(None);

pub(crate) fn info(message: impl Display) {
    record(Level::Info, message);
}

pub(crate) fn warning(message: impl Display) {
    record(Level::Warning, message);
}

pub(crate) fn error(message: impl Display) {
    record(Level::Error, message);
}

/// An error followed by each of its sources, joined by `: `, the way `{:#}` prints an
/// `anyhow::Error`.
pub(crate) fn chain<'a>(error: &'a (dyn std::error::Error + 'static)) -> impl Display + 'a {
    Chain(error)
}

struct Chain<'a>(&'a (dyn std::error::Error + 'static));

impl Display for Chain<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(formatter, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

pub(crate) fn since(sequence: u64) -> Vec<Entry> {
    journal()
        .entries
        .iter()
        .filter(|entry| entry.sequence > sequence)
        .cloned()
        .collect()
}

pub(crate) fn mirror_to(path: &Path) -> std::io::Result<()> {
    let mut created = File::create(path)?;
    let mut file = mirror();
    // Holding the mirror lock keeps entries from slipping between the copy and the switch.
    let lines: Vec<String> = journal().entries.iter().map(Entry::line).collect();
    for line in lines {
        writeln!(created, "{line}")?;
    }
    *file = Some(created);
    Ok(())
}

fn record(level: Level, message: impl Display) {
    let time = timestamp();
    let message = message.to_string();
    let mut file = mirror();
    let line = journal().append(time, level, message);
    if let Some(file) = file.as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

fn journal() -> MutexGuard<'static, Journal> {
    JOURNAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn mirror() -> MutexGuard<'static, Option<File>> {
    MIRROR.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
fn detach_mirror() -> Option<File> {
    mirror().take()
}

fn timestamp() -> String {
    let now = local_time();
    format!("{:02}:{:02}:{:02}", now.hour, now.minute, now.second)
}

pub(crate) fn file_stamp() -> String {
    let now = local_time();
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

pub(crate) fn today() -> String {
    let now = local_time();
    format!("{:04}-{:02}-{:02}", now.year, now.month, now.day)
}

/// The wall clock on Windows, UTC elsewhere.
struct LocalTime {
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
}

#[cfg(windows)]
fn local_time() -> LocalTime {
    let now = winsafe::GetLocalTime();
    LocalTime {
        year: i32::from(now.wYear),
        month: i32::from(now.wMonth),
        day: i32::from(now.wDay),
        hour: i32::from(now.wHour),
        minute: i32::from(now.wMinute),
        second: i32::from(now.wSecond),
    }
}

#[cfg(not(windows))]
fn local_time() -> LocalTime {
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    LocalTime {
        year: i32::from(now.year()),
        month: i32::from(now.month()),
        day: i32::from(now.day()),
        hour: i32::from(now.hour()),
        minute: i32::from(now.minute()),
        second: i32::from(now.second()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(detach_mirror().is_some());
        directory.close().unwrap();
    }
}
