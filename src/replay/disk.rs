//! The replay buffer in one unbuffered file on disk, written by a thread of its own so the
//! capture thread never waits on the drive; reads lay the staged blocks over the file's bytes.

use super::store::{
    CHUNK_BYTES, MEBIBYTE, PAGE_BYTES, Page, Pages, Slot, Store, StoreError, reserved_pages,
};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread,
};
use thiserror::Error;

/// `FILE_FLAG_NO_BUFFERING` needs every offset, length and buffer aligned to the sector; 4 KiB
/// is the largest in common use.
const SECTOR_BYTES: usize = 4096;
const BLOCK_BYTES: usize = 1 << 20;
const MAX_QUEUED_BYTES: usize = 64 << 20;
const SPARE_BLOCKS: usize = 2;
const KEPT_FREE_BYTES: u64 = 1 << 30;
const WRITER_THREAD: &str = "buffer writer";

#[derive(Debug, Error)]
pub(crate) enum DiskError {
    #[error("create the folder {}: {source}", path.display())]
    Folder {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("read the drive's free space: {0}")]
    FreeSpace(#[source] io::Error),
    #[error(
        "the buffer would take {} MiB and the drive has {} MiB free, {} MiB of which stay free",
        needed / MEBIBYTE as u64,
        available / MEBIBYTE as u64,
        KEPT_FREE_BYTES / MEBIBYTE as u64
    )]
    NoRoom { needed: u64, available: u64 },
    #[error("create {}: {source}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("reserve the file's room: {0}")]
    Reserve(#[source] io::Error),
    /// Fails where the drive's sectors are larger than [`SECTOR_BYTES`].
    #[error("write to the file without buffering: {0}")]
    Unbuffered(#[source] io::Error),
    #[error("start the buffer's writer thread: {0}")]
    Writer(#[source] io::Error),
}

/// Sizes in bytes, all multiples of [`SECTOR_BYTES`]; tests shrink them.
#[derive(Clone, Copy, Debug)]
struct Layout {
    page: usize,
    block: usize,
    piece: usize,
    max_queued: usize,
}

const LAYOUT: Layout = Layout {
    page: PAGE_BYTES,
    block: BLOCK_BYTES,
    piece: CHUNK_BYTES,
    max_queued: MAX_QUEUED_BYTES,
};

pub(crate) struct DiskStore {
    shared: Arc<Shared>,
    pages: Pages<DiskPage>,
    /// File offset of the next page made beyond the reserved ones.
    end: u64,
    path: PathBuf,
}

impl DiskStore {
    /// Reserves the file for `bytes` of packets plus a page, replacing a previous buffer's file.
    pub(crate) fn create(path: &Path, bytes: u64) -> Result<Self, DiskError> {
        Self::with_layout(path, reserved_pages(bytes), LAYOUT)
    }

    fn with_layout(path: &Path, pages: usize, layout: Layout) -> Result<Self, DiskError> {
        let folder = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(folder).map_err(|source| DiskError::Folder {
            path: folder.to_owned(),
            source,
        })?;
        // Removed first so its room counts as free; a stuck file fails the create below instead.
        let _ = fs::remove_file(path);
        let length = (pages as u64).saturating_mul(layout.page as u64);
        has_room(length, free_bytes(folder).map_err(DiskError::FreeSpace)?)?;
        let file = open(path).map_err(|source| DiskError::Create {
            path: path.to_owned(),
            source,
        })?;
        file.set_len(length).map_err(DiskError::Reserve)?;
        try_unbuffered(&file).map_err(DiskError::Unbuffered)?;

        let shared = Arc::new(Shared {
            file,
            layout,
            staging: Mutex::default(),
            queued: Condvar::new(),
        });
        let writer = Arc::clone(&shared);
        thread::Builder::new()
            .name(WRITER_THREAD.to_owned())
            .spawn(move || writer.write_blocks())
            .map_err(DiskError::Writer)?;

        // `Pages` pops free pages from the back; reversed, the file fills from its start, so no
        // write lands past unwritten space that Windows would zero-fill first.
        let page_bytes = layout.page as u64;
        let reserved = (0..pages as u64)
            .rev()
            .map(|index| DiskPage {
                shared: Arc::clone(&shared),
                base: index * page_bytes,
            })
            .collect();
        Ok(Self {
            shared,
            pages: Pages::new(layout.page, reserved),
            end: length,
            path: path.to_owned(),
        })
    }

    pub(crate) fn description(&self) -> String {
        let layout = self.shared.layout;
        format!(
            "Replay buffer on disk: {} in {}, written without the file cache in {} MiB blocks",
            self.pages.summary(),
            self.path.display(),
            layout.block / MEBIBYTE
        )
    }
}

impl Store for DiskStore {
    fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError> {
        let Self {
            shared, pages, end, ..
        } = self;
        let page_bytes = shared.layout.page as u64;
        let (page, offset) = pages.place(data.len(), || {
            // Past the file's end; the writer's first block there lengthens the file.
            let page = DiskPage {
                shared: Arc::clone(shared),
                base: *end,
            };
            *end += page_bytes;
            Ok(page)
        })?;
        shared.stage(page.base + offset as u64, data)?;
        Ok(Slot::new(page, offset, data.len()))
    }
}

impl Drop for DiskStore {
    /// The file stays until the pages a save may still be reading let go of it.
    fn drop(&mut self) {
        self.shared.close();
    }
}

struct DiskPage {
    shared: Arc<Shared>,
    base: u64,
}

impl Page for DiskPage {
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        self.shared.read(self.base, offset, length, sink)
    }
}

struct Shared {
    file: File,
    layout: Layout,
    staging: Mutex<Staging>,
    queued: Condvar,
}

#[derive(Default)]
struct Staging {
    filling: Option<Block>,
    /// Oldest first; a block leaves only once it is in the file.
    queue: VecDeque<Arc<Block>>,
    queued_bytes: usize,
    spare: Vec<Aligned>,
    failed: Option<Arc<io::Error>>,
    closing: bool,
    /// Holds the writer back.
    #[cfg(test)]
    held: bool,
}

struct Block {
    position: u64,
    memory: Aligned,
    used: usize,
}

impl Block {
    fn end(&self) -> u64 {
        self.position + self.used as u64
    }

    fn bytes(&self) -> &[u8] {
        &self.memory.bytes()[..self.used]
    }

    fn is_full(&self) -> bool {
        self.used == self.memory.len()
    }

    fn overlaps(&self, from: u64, to: u64) -> bool {
        self.position < to && self.end() > from
    }

    fn append(&mut self, data: &[u8]) -> usize {
        let taken = data.len().min(self.memory.len() - self.used);
        self.memory.bytes_mut()[self.used..self.used + taken].copy_from_slice(&data[..taken]);
        self.used += taken;
        taken
    }

    /// The used bytes rounded up to a sector; the slack lies past the page's packets, so nothing
    /// reads it.
    fn sectors(&self) -> &[u8] {
        &self.memory.bytes()[..self.used.next_multiple_of(SECTOR_BYTES)]
    }
}

impl Shared {
    fn staging(&self) -> MutexGuard<'_, Staging> {
        self.staging.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stage(&self, mut position: u64, mut data: &[u8]) -> Result<(), StoreError> {
        let mut staging = self.staging();
        if let Some(error) = &staging.failed {
            return Err(StoreError::call("write the buffer file")(Arc::clone(error)));
        }
        let queued = staging.queue.len();
        while !data.is_empty() {
            let block = staging.block_at(position, &self.layout)?;
            let taken = block.append(data);
            let full = block.is_full();
            position += taken as u64;
            data = &data[taken..];
            if full {
                staging.queue_filling(&self.layout)?;
            }
        }
        if staging.queue.len() > queued {
            self.queued.notify_one();
        }
        Ok(())
    }

    /// A block leaves the queue only once it is in the file, so a read finds every byte in one
    /// or the other.
    fn write_blocks(&self) {
        while let Some(block) = self.next_block() {
            let written = write_at(&self.file, block.sectors(), block.position);
            let mut staging = self.staging();
            if let Err(error) = written {
                staging.failed = Some(Arc::new(error));
                return;
            }
            staging.queue.pop_front();
            staging.queued_bytes -= block.memory.len();
            if let Some(block) = Arc::into_inner(block)
                && staging.spare.len() < SPARE_BLOCKS
            {
                staging.spare.push(block.memory);
            }
        }
    }

    fn next_block(&self) -> Option<Arc<Block>> {
        let mut staging = self.staging();
        loop {
            if let Some(block) = staging.ready() {
                return Some(Arc::clone(block));
            }
            if staging.closing {
                return None;
            }
            staging = self
                .queued
                .wait(staging)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut staging = self.staging();
        if let Some(block) = staging.filling.take() {
            staging.queued_bytes += block.memory.len();
            staging.queue.push_back(Arc::new(block));
        }
        staging.closing = true;
        self.queued.notify_one();
    }

    fn read(
        &self,
        base: u64,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        if length == 0 {
            return Ok(());
        }
        // A page starts on a sector boundary, so sectors are counted within it.
        let (start, end) = (offset, offset + length);
        let (first, last) = (
            start - start % SECTOR_BYTES,
            end.next_multiple_of(SECTOR_BYTES),
        );
        let mut memory = Aligned::new(self.layout.piece.min(last - first));
        let mut at = first;
        while at < last {
            let piece_end = (at + memory.len()).min(last);
            let piece = &mut memory.bytes_mut()[..piece_end - at];
            let position = base + at as u64;
            // Whatever is not in staging now is in the file already.
            let staged = self.staged(position, position + piece.len() as u64);
            read_at(&self.file, piece, position)
                .map_err(StoreError::call("read the buffer file"))?;
            for block in &staged {
                block.lay_over(piece, position);
            }
            let (from, to) = (start.max(at), end.min(piece_end));
            sink(from - start, &piece[from - at..to - at]);
            at = piece_end;
        }
        Ok(())
    }

    /// Queued blocks as they are; the one being filled copied, since it is still changing.
    fn staged(&self, from: u64, to: u64) -> Vec<Staged> {
        let staging = self.staging();
        let mut staged: Vec<Staged> = staging
            .queue
            .iter()
            .filter(|block| block.overlaps(from, to))
            .map(|block| Staged::Queued(Arc::clone(block)))
            .collect();
        if let Some(block) = &staging.filling
            && block.overlaps(from, to)
        {
            staged.push(Staged::Filling {
                position: block.position,
                bytes: block.bytes().to_vec(),
            });
        }
        staged
    }
}

impl Staging {
    /// The oldest queued block, unless a test holds the writer back.
    fn ready(&self) -> Option<&Arc<Block>> {
        #[cfg(test)]
        if self.held {
            return None;
        }
        self.queue.front()
    }

    /// A page's packets are contiguous, so a new block starts only where a page does.
    fn block_at(&mut self, position: u64, layout: &Layout) -> Result<&mut Block, StoreError> {
        let follows = self
            .filling
            .as_ref()
            .is_some_and(|block| block.end() == position);
        if !follows {
            self.queue_filling(layout)?;
            debug_assert!(position.is_multiple_of(SECTOR_BYTES as u64));
            let memory = self
                .spare
                .pop()
                .unwrap_or_else(|| Aligned::new(layout.block));
            self.filling = Some(Block {
                position,
                memory,
                used: 0,
            });
        }
        Ok(self
            .filling
            .as_mut()
            .expect("a block is being filled or was just started"))
    }

    /// Refuses when the queue is full; the block stays put so reads still find its bytes.
    fn queue_filling(&mut self, layout: &Layout) -> Result<(), StoreError> {
        let Some(block) = self.filling.take() else {
            return Ok(());
        };
        if self.queued_bytes + block.memory.len() > layout.max_queued {
            self.filling = Some(block);
            return Err(StoreError::Refused(format!(
                "the drive fell {} MiB behind the buffer",
                self.queued_bytes / MEBIBYTE
            )));
        }
        self.queued_bytes += block.memory.len();
        self.queue.push_back(Arc::new(block));
        Ok(())
    }
}

enum Staged {
    Queued(Arc<Block>),
    Filling { position: u64, bytes: Vec<u8> },
}

impl Staged {
    fn lay_over(&self, piece: &mut [u8], at: u64) {
        let (position, bytes) = match self {
            Self::Queued(block) => (block.position, block.bytes()),
            Self::Filling { position, bytes } => (*position, &bytes[..]),
        };
        let from = position.max(at);
        let to = (position + bytes.len() as u64).min(at + piece.len() as u64);
        if from < to {
            piece[distance(at, from)..distance(at, to)]
                .copy_from_slice(&bytes[distance(position, from)..distance(position, to)]);
        }
    }
}

fn distance(from: u64, to: u64) -> usize {
    usize::try_from(to - from).expect("a block or a piece fits in memory")
}

/// Sector-aligned memory: a vector with a sector to spare, used from its first boundary on.
struct Aligned {
    memory: Vec<u8>,
    start: usize,
    length: usize,
}

impl Aligned {
    fn new(length: usize) -> Self {
        let memory = vec![0; length + SECTOR_BYTES];
        let start = memory.as_ptr().addr().wrapping_neg() % SECTOR_BYTES;
        Self {
            memory,
            start,
            length,
        }
    }

    fn len(&self) -> usize {
        self.length
    }

    fn bytes(&self) -> &[u8] {
        &self.memory[self.start..self.start + self.length]
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.memory[self.start..self.start + self.length]
    }
}

fn has_room(needed: u64, available: u64) -> Result<(), DiskError> {
    if needed
        .checked_add(KEPT_FREE_BYTES)
        .is_none_or(|total| total > available)
    {
        return Err(DiskError::NoRoom { needed, available });
    }
    Ok(())
}

/// Makes a drive that refuses unbuffered transfers fail now, not once the buffer is in use.
fn try_unbuffered(file: &File) -> io::Result<()> {
    let mut sector = Aligned::new(SECTOR_BYTES);
    write_at(file, sector.bytes(), 0)?;
    read_at(file, sector.bytes_mut(), 0)
}

/// Past the file's end (a page not yet written) the bytes are zeros.
fn read_at(file: &File, buffer: &mut [u8], position: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buffer.len() {
        match read_once(file, &mut buffer[done..], position + done as u64) {
            Ok(0) => {
                buffer[done..].fill(0);
                break;
            }
            Ok(read) => done += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_at(file: &File, bytes: &[u8], position: u64) -> io::Result<()> {
    let mut done = 0;
    while done < bytes.len() {
        match write_once(file, &bytes[done..], position + done as u64) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => done += written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Only deletion is shared, so the next session's buffer can replace the file while a save
/// still reads this one; `FILE_FLAG_WRITE_THROUGH` is left out on purpose.
#[cfg(windows)]
fn open(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_NO_BUFFERING, FILE_SHARE_DELETE,
    };
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_DELETE_ON_CLOSE.0)
        .open(path)
}

/// Tests only: unlinked at once, and there is no unbuffered mode to ask for.
#[cfg(not(windows))]
fn open(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    fs::remove_file(path)?;
    Ok(file)
}

#[cfg(windows)]
fn free_bytes(folder: &Path) -> io::Result<u64> {
    crate::sys::volume::free_bytes(folder)
}

#[cfg(not(windows))]
#[expect(clippy::unnecessary_wraps)]
fn free_bytes(_folder: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

#[cfg(windows)]
fn read_once(file: &File, buffer: &mut [u8], position: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, position)
}

#[cfg(not(windows))]
fn read_once(file: &File, buffer: &mut [u8], position: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, position)
}

#[cfg(windows)]
fn write_once(file: &File, bytes: &[u8], position: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, bytes, position)
}

#[cfg(not(windows))]
fn write_once(file: &File, bytes: &[u8], position: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, bytes, position)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        super::store::{Data, Entry, fetch},
        *,
    };
    use std::{assert_matches, time::Duration};

    const SMALL: Layout = Layout {
        page: 4 * SECTOR_BYTES,
        block: SECTOR_BYTES,
        piece: 2 * SECTOR_BYTES,
        max_queued: 16 * SECTOR_BYTES,
    };

    impl DiskStore {
        pub(crate) fn small(folder: &Path, pages: usize) -> Self {
            Self::sized(folder, pages, SMALL)
        }

        fn sized(folder: &Path, pages: usize, layout: Layout) -> Self {
            Self::with_layout(&folder.join("buffer.bin"), pages, layout).unwrap()
        }

        pub(crate) fn control(&self) -> Control {
            Control(Arc::clone(&self.shared))
        }
    }

    pub(crate) struct Control(Arc<Shared>);

    impl Control {
        pub(crate) fn hold(&self, held: bool) {
            self.0.staging().held = held;
            self.0.queued.notify_one();
        }

        pub(crate) fn settle(&self) {
            while !self.0.staging().queue.is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
        }

        pub(crate) fn queued(&self) -> usize {
            self.0.staging().queue.len()
        }

        fn filling(&self) -> Option<usize> {
            self.0.staging().filling.as_ref().map(|block| block.used)
        }
    }

    fn packet(seed: u8, length: usize) -> Vec<u8> {
        (0..length)
            .map(|at| {
                seed.wrapping_mul(31)
                    .wrapping_add(u8::try_from(at % 251).unwrap())
            })
            .collect()
    }

    fn put(store: &mut DiskStore, index: u64, data: &[u8]) -> Arc<Entry> {
        Arc::new(Entry {
            index,
            keyframe: index == 0,
            data: Data::Stored(store.put(data).unwrap()),
        })
    }

    fn put_all(store: &mut DiskStore, packets: &[Vec<u8>]) -> Vec<Arc<Entry>> {
        (0..)
            .zip(packets)
            .map(|(index, data)| put(store, index, data))
            .collect()
    }

    fn read_back(entries: &[Arc<Entry>]) -> Vec<Vec<u8>> {
        fetch(entries)
            .unwrap()
            .iter()
            .map(|packet| packet.data.to_vec())
            .collect()
    }

    #[test]
    fn packets_are_read_back_from_staging_before_they_reach_the_file() {
        let folder = tempfile::tempdir().unwrap();
        let mut store = DiskStore::small(folder.path(), 3);
        store.control().hold(true);
        let packets: Vec<Vec<u8>> = (0..12)
            .map(|seed| packet(seed, 700 + 300 * usize::from(seed)))
            .collect();
        let entries = put_all(&mut store, &packets);
        assert!(
            store.control().queued() > 1,
            "full blocks wait for the held writer"
        );
        assert_eq!(read_back(&entries), packets);

        store.control().hold(false);
        store.control().settle();
        assert_eq!(read_back(&entries), packets, "from the file");
    }

    #[test]
    fn a_packet_is_read_across_the_file_and_the_block_being_filled() {
        let folder = tempfile::tempdir().unwrap();
        let mut store = DiskStore::small(folder.path(), 2);
        let first = packet(1, 2 * SECTOR_BYTES + SECTOR_BYTES / 2);
        let mut entries = vec![put(&mut store, 0, &first)];
        store.control().settle();
        assert_eq!(store.control().filling(), Some(SECTOR_BYTES / 2));
        assert_eq!(read_back(&entries), std::slice::from_ref(&first));

        let (second, third) = (packet(2, 100), packet(3, 2 * SECTOR_BYTES));
        entries.push(put(&mut store, 1, &second));
        entries.push(put(&mut store, 2, &third));
        assert_matches!(&entries[2].data, Data::Stored(slot) if slot.offset() == 0);
        assert_eq!(read_back(&entries), [first, second, third]);
    }

    #[test]
    fn a_page_is_filled_again_once_nothing_holds_it() {
        let folder = tempfile::tempdir().unwrap();
        let mut store = DiskStore::small(folder.path(), 2);
        store.control().hold(true);
        let length = SECTOR_BYTES + 100;
        let old = put_all(&mut store, &vec![packet(0, length); 3]);
        drop(old);
        let packets: Vec<Vec<u8>> = (1..7).map(|seed| packet(seed, length)).collect();
        let entries = put_all(&mut store, &packets);
        assert_eq!(store.pages.count(), 2, "the first page was filled again");
        assert_eq!(read_back(&entries), packets);

        store.control().hold(false);
        store.control().settle();
        assert_eq!(read_back(&entries), packets, "from the file");
    }

    #[test]
    fn pages_beyond_the_reserved_ones_extend_the_file() {
        let folder = tempfile::tempdir().unwrap();
        let mut store = DiskStore::small(folder.path(), 1);
        store.control().hold(true);
        let packets: Vec<Vec<u8>> = (0..4).map(|seed| packet(seed, 3 * SECTOR_BYTES)).collect();
        let entries = put_all(&mut store, &packets);
        assert_eq!(store.pages.count(), 4);
        assert_eq!(read_back(&entries), packets);

        store.control().hold(false);
        store.control().settle();
        assert_eq!(read_back(&entries), packets, "from the file");
    }

    #[test]
    fn the_store_refuses_packets_once_the_drive_falls_behind() {
        let folder = tempfile::tempdir().unwrap();
        let layout = Layout {
            max_queued: 2 * SECTOR_BYTES,
            ..SMALL
        };
        let mut store = DiskStore::sized(folder.path(), 2, layout);
        store.control().hold(true);
        let kept = put(&mut store, 0, &packet(0, 2 * SECTOR_BYTES));
        assert_matches!(
            store.put(&packet(1, 2 * SECTOR_BYTES)),
            Err(StoreError::Refused(_))
        );
        assert_eq!(read_back(&[kept]), [packet(0, 2 * SECTOR_BYTES)]);
    }

    #[test]
    fn the_file_goes_once_the_store_and_its_pages_do() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("buffer.bin");
        fs::write(&path, b"left by an earlier run").unwrap();
        let mut store = DiskStore::small(folder.path(), 2);
        let kept = put(&mut store, 0, &packet(0, 5000));
        drop(store);
        assert_eq!(read_back(std::slice::from_ref(&kept)), [packet(0, 5000)]);

        let mut next = DiskStore::small(folder.path(), 2);
        let fresh = put(&mut next, 0, &packet(1, 5000));
        assert_eq!(read_back(&[kept]), [packet(0, 5000)]);
        drop(next);
        assert_eq!(read_back(&[fresh]), [packet(1, 5000)]);

        for _ in 0..1000 {
            if !path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("{} outlived its buffer", path.display());
    }

    #[test]
    fn the_buffer_leaves_a_gigabyte_of_the_drive_free() {
        assert!(has_room(1 << 20, KEPT_FREE_BYTES + (1 << 20)).is_ok());
        assert_matches!(
            has_room(1 << 20, KEPT_FREE_BYTES),
            Err(DiskError::NoRoom { .. })
        );
        assert_matches!(has_room(u64::MAX, u64::MAX), Err(DiskError::NoRoom { .. }));
    }

    #[test]
    fn staging_memory_starts_on_a_sector_boundary() {
        for length in [SECTOR_BYTES, 3 * SECTOR_BYTES] {
            let memory = Aligned::new(length);
            assert_eq!(memory.bytes().as_ptr().addr() % SECTOR_BYTES, 0);
            assert_eq!(memory.len(), length);
        }
    }
}
