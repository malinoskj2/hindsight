//! The replay buffer on disk: packets go into 64 MiB pages that are regions of one file,
//! reserved up front at the buffer's estimated size plus a page. The file lasts as long as the
//! buffer (Windows deletes it when its last handle closes, so not even a crash leaves it
//! behind), and the next session's buffer replaces it.
//!
//! The capture thread never waits on the drive. A packet is copied into a staging block in
//! memory, and a writer thread of the store's own writes each full block to the file. A save
//! may come right after a packet arrives, so a read takes what the file holds and lays over it
//! every block that has not reached the file yet. The buffer's footprint in memory is the block
//! being filled, those waiting for the writer (one at most while the drive keeps up) and two
//! kept for reuse: a few MiB.
//!
//! The file is opened with `FILE_FLAG_NO_BUFFERING`, so the blocks go to the drive rather than
//! into the Windows file cache, which would otherwise keep the most recently written part of
//! the buffer in memory, the very memory a buffer on disk is meant to leave free. Unbuffered
//! transfers must start, end and lie in memory on sector boundaries; blocks, pages and read
//! pieces are all whole multiples of 4 KiB, the largest sector size in common use, and are held
//! in memory aligned to it. `FILE_FLAG_WRITE_THROUGH` is left out: it makes the drive commit
//! each write past its own cache, a durability a file deleted at the end of the session does
//! not need, paid for in speed and wear.

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

/// The alignment unbuffered I/O asks of every transfer's offset, length and memory: 4 KiB,
/// the largest sector size drives commonly report.
const SECTOR_BYTES: usize = 4096;
/// Bytes in a staging block: large enough that the drive sees long sequential writes, small
/// enough that the blocks in memory stay a few MiB.
const BLOCK_BYTES: usize = 1 << 20;
/// Bytes of full blocks that may wait for the writer before the store refuses packets: over
/// three seconds at the highest bitrate, longer than a drive that keeps up ever falls behind.
const MAX_QUEUED_BYTES: usize = 64 << 20;
/// Blocks kept for the next packets once written, so that the capture thread seldom allocates.
const SPARE_BLOCKS: usize = 2;
/// Free space the buffer leaves on its drive, so that reserving it never fills the drive.
const KEPT_FREE_BYTES: u64 = 1 << 30;
/// The writer thread's name, for debuggers.
const WRITER_THREAD: &str = "buffer writer";

/// Why the buffer could not be placed on disk.
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
    /// The drive lacks room for the buffer and the space left free beside it.
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
    /// A first unbuffered write and read back failed, as they do where the drive's sectors are
    /// larger than the transfers are aligned to.
    #[error("write to the file without buffering: {0}")]
    Unbuffered(#[source] io::Error),
    #[error("start the buffer's writer thread: {0}")]
    Writer(#[source] io::Error),
}

/// The sizes a store works in, in bytes and as multiples of [`SECTOR_BYTES`], a page holding
/// whole blocks: of a page, of a staging block, of a piece a read reads at a time, and of the
/// blocks that may wait for the writer. Tests make them small.
#[derive(Clone, Copy, Debug)]
struct Layout {
    page: usize,
    block: usize,
    piece: usize,
    max_queued: usize,
}

/// The sizes a store is made with.
const LAYOUT: Layout = Layout {
    page: PAGE_BYTES,
    block: BLOCK_BYTES,
    piece: CHUNK_BYTES,
    max_queued: MAX_QUEUED_BYTES,
};

/// The buffer's pages in a file on disk.
pub(crate) struct DiskStore {
    shared: Arc<Shared>,
    pages: Pages<DiskPage>,
    /// Where a page made beyond the reserved ones begins: past the last page so far.
    end: u64,
    /// Pages reserved up front.
    reserved: usize,
    path: PathBuf,
}

impl DiskStore {
    /// Reserves a file at `path` with pages for `bytes` of packets, and one more for the group
    /// of pictures the buffer keeps beyond its span, replacing the file a previous buffer left.
    /// The pages are on the file's drive only if it has room for them and a gigabyte besides.
    /// More pages are added at the file's end as needed if the packets outgrow the estimate.
    pub(crate) fn create(path: &Path, bytes: u64) -> Result<Self, DiskError> {
        Self::with_layout(path, reserved_pages(bytes), LAYOUT)
    }

    fn with_layout(path: &Path, pages: usize, layout: Layout) -> Result<Self, DiskError> {
        let folder = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(folder).map_err(|source| DiskError::Folder {
            path: folder.to_owned(),
            source,
        })?;
        // The previous buffer's file goes first, so that its room counts as free. A file that
        // cannot be removed makes the creation below fail, which reports it.
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

        // `Pages` fills the last of its free pages first: listed backwards, the pages are first
        // filled from the file's start on, so a write never lands past a stretch that was never
        // written, which Windows would fill with zeros first.
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
            reserved: pages,
            path: path.to_owned(),
        })
    }

    /// What was reserved, for the log.
    pub(crate) fn description(&self) -> String {
        let layout = self.shared.layout;
        format!(
            "Replay buffer on disk: {} MiB reserved in {} MiB pages in {}, written without the file cache in {} MiB blocks",
            self.reserved * layout.page / MEBIBYTE,
            layout.page / MEBIBYTE,
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
            // Past the file's end: the writer's first block there makes the file longer.
            let page = DiskPage {
                shared: Arc::clone(shared),
                base: *end,
            };
            *end += page_bytes;
            Ok(page)
        })?;
        shared.stage(page.base + offset as u64, data)?;
        Ok(Pages::slot(page, offset, data.len()))
    }
}

impl Drop for DiskStore {
    /// Lets the writer write what is staged and stop. The file stays until the pages a save
    /// may still be reading let go of it.
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// One page: the region of the file from `base` on.
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

/// What the store, its pages and its writer share: the file and the blocks not yet in it.
struct Shared {
    file: File,
    layout: Layout,
    staging: Mutex<Staging>,
    /// Wakes the writer when a block is queued or the store closes.
    queued: Condvar,
}

/// The blocks on their way to the file.
#[derive(Default)]
struct Staging {
    /// The block packets are being copied into.
    filling: Option<Block>,
    /// Blocks for the writer, oldest first; the first stays here until it is written.
    queue: VecDeque<Arc<Block>>,
    /// Bytes of the blocks in `queue`.
    queued_bytes: usize,
    /// Written blocks' memory, kept for the next ones.
    spare: Vec<Aligned>,
    /// Why the writer stopped, in words; the store refuses packets from then on.
    failed: Option<String>,
    /// The store is gone: the writer stops once the queue is empty.
    closing: bool,
    /// Holds the writer back, so tests can read blocks that have not reached the file.
    held: bool,
}

/// A run of bytes for the file at `position`, a sector boundary: the first `used` bytes of
/// `memory`.
struct Block {
    position: u64,
    memory: Aligned,
    used: usize,
}

impl Block {
    /// Where the block's bytes end in the file.
    fn end(&self) -> u64 {
        self.position + self.used as u64
    }

    fn bytes(&self) -> &[u8] {
        &self.memory.bytes()[..self.used]
    }

    fn is_full(&self) -> bool {
        self.used == self.memory.len()
    }

    /// Whether the block has bytes in the file's range from `from` to `to`.
    fn overlaps(&self, from: u64, to: u64) -> bool {
        self.position < to && self.end() > from
    }

    /// Copies as much of `data` as fits after the bytes in use, and says how much that was.
    fn append(&mut self, data: &[u8]) -> usize {
        let taken = data.len().min(self.memory.len() - self.used);
        self.memory.bytes_mut()[self.used..self.used + taken].copy_from_slice(&data[..taken]);
        self.used += taken;
        taken
    }

    /// What the writer writes: the bytes in use, to the next sector boundary. The bytes past
    /// those in use lie past the packets of the block's page, so nothing reads them.
    fn sectors(&self) -> &[u8] {
        &self.memory.bytes()[..self.used.next_multiple_of(SECTOR_BYTES)]
    }
}

impl Shared {
    fn staging(&self) -> MutexGuard<'_, Staging> {
        self.staging.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Copies `data`, for the file at `position`, into the staging blocks, queueing each block
    /// it fills for the writer. Takes the staging lock only for the copy; the capture thread
    /// never waits on the drive.
    fn stage(&self, mut position: u64, mut data: &[u8]) -> Result<(), StoreError> {
        let mut staging = self.staging();
        if let Some(failure) = &staging.failed {
            return Err(refused(failure.clone()));
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

    /// Writes the queued blocks to the file, oldest first, until the store closes and the queue
    /// is empty, or a write fails. A block leaves the queue only once it is in the file, so a
    /// read finds every byte in one or the other.
    fn write_blocks(&self) {
        while let Some(block) = self.next_block() {
            let written = write_at(&self.file, block.sectors(), block.position);
            let mut staging = self.staging();
            if let Err(error) = written {
                staging.failed = Some(format!("write the buffer file: {error}"));
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

    /// The oldest queued block, once there is one; `None` once the store has closed and the
    /// queue is empty.
    fn next_block(&self) -> Option<Arc<Block>> {
        let mut staging = self.staging();
        loop {
            if !staging.held
                && let Some(block) = staging.queue.front()
            {
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

    /// Queues the block being filled, whatever its size and the queue's, and tells the writer
    /// to stop once it has written the queue.
    fn close(&self) {
        let mut staging = self.staging();
        if let Some(block) = staging.filling.take() {
            staging.queued_bytes += block.memory.len();
            staging.queue.push_back(Arc::new(block));
        }
        staging.closing = true;
        self.queued.notify_one();
    }

    /// Reads the `length` bytes at `offset` of the page at `base` and hands them to `sink` in
    /// pieces, each read from the file with the staged blocks laid over it.
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
        // A page starts on a sector boundary, so the sectors are counted within it.
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

    /// The staged bytes in the file's range from `from` to `to`, oldest first: the queued
    /// blocks as they are, the one being filled copied, as it is still changing.
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
    /// The block for bytes at `position`: the one being filled, when they follow its bytes;
    /// otherwise a new one, the one being filled queued. A page's packets follow one another,
    /// so a new block starts only where a page does, on a sector boundary.
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

    /// Hands the block being filled to the writer, unless the queue is full: the drive has
    /// fallen too far behind, and the store refuses the packet. The block stays where it is,
    /// so that reads still find its bytes.
    fn queue_filling(&mut self, layout: &Layout) -> Result<(), StoreError> {
        let Some(block) = self.filling.take() else {
            return Ok(());
        };
        if self.queued_bytes + block.memory.len() > layout.max_queued {
            self.filling = Some(block);
            return Err(refused(format!(
                "the drive fell {} MiB behind the buffer",
                self.queued_bytes / MEBIBYTE
            )));
        }
        self.queued_bytes += block.memory.len();
        self.queue.push_back(Arc::new(block));
        Ok(())
    }
}

/// Bytes that had not reached the file when a read began.
enum Staged {
    Queued(Arc<Block>),
    Filling { position: u64, bytes: Vec<u8> },
}

impl Staged {
    /// Copies the bytes over those of `piece`, the file's bytes from `at`, where they overlap.
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

/// How far `to` lies past `from`, both in one block or piece.
fn distance(from: u64, to: u64) -> usize {
    usize::try_from(to - from).expect("a block or a piece fits in memory")
}

/// The store's refusal of a packet, for `reason`.
fn refused(reason: String) -> StoreError {
    StoreError::call("stage the packet")(io::Error::other(reason))
}

/// Memory that starts on a sector boundary, as unbuffered transfers need: a vector with a
/// sector's room to spare, used from its first boundary on.
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

/// Fails unless `available` bytes leave room for `needed` and the space kept free.
fn has_room(needed: u64, available: u64) -> Result<(), DiskError> {
    if needed
        .checked_add(KEPT_FREE_BYTES)
        .is_none_or(|total| total > available)
    {
        return Err(DiskError::NoRoom { needed, available });
    }
    Ok(())
}

/// Writes a sector to the start of `file` and reads it back, so that a drive that refuses the
/// unbuffered transfers does so now, not once the buffer is in use.
fn try_unbuffered(file: &File) -> io::Result<()> {
    let mut sector = Aligned::new(SECTOR_BYTES);
    write_at(file, sector.bytes(), 0)?;
    read_at(file, sector.bytes_mut(), 0)
}

/// Reads `buffer` full from `file` at `position`. Past the file's end, where a page made
/// beyond the reserved ones has not been written yet, the bytes are zeros.
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

/// Writes all of `bytes` to `file` at `position`.
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

/// Opens the buffer's file at `path`, empty, for unbuffered reads and writes, to be deleted
/// when its last handle closes. Only deletion is shared, so the next session's buffer can
/// replace the file while a save still reads this one.
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

/// Opens the buffer's file at `path`, empty, and unlinks it at once: it lasts as long as its
/// handle. There is no unbuffered mode to ask for here; this is for the tests.
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

/// The free space is not checked here; the store is only tested.
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
        super::store::{Entry, fetch},
        *,
    };
    use std::{assert_matches, time::Duration};

    /// Pages of four sectors, blocks of one, read back two at a time: a scale model of the
    /// real sizes, so that packets cross blocks, pieces and pages.
    const SMALL: Layout = Layout {
        page: 4 * SECTOR_BYTES,
        block: SECTOR_BYTES,
        piece: 2 * SECTOR_BYTES,
        max_queued: 16 * SECTOR_BYTES,
    };

    impl DiskStore {
        /// A store of `pages` small pages in `buffer.bin` in `folder`.
        pub(crate) fn small(folder: &Path, pages: usize) -> Self {
            Self::sized(folder, pages, SMALL)
        }

        fn sized(folder: &Path, pages: usize, layout: Layout) -> Self {
            Self::with_layout(&folder.join("buffer.bin"), pages, layout).unwrap()
        }

        /// A handle on the store's writer, which outlives handing the store over.
        pub(crate) fn control(&self) -> Control {
            Control(Arc::clone(&self.shared))
        }
    }

    /// A handle on a store's writer and staging.
    pub(crate) struct Control(Arc<Shared>);

    impl Control {
        /// Holds the writer back, or lets it go on.
        pub(crate) fn hold(&self, held: bool) {
            self.0.staging().held = held;
            self.0.queued.notify_one();
        }

        /// Waits for the writer to write every queued block.
        pub(crate) fn settle(&self) {
            while !self.0.staging().queue.is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Blocks waiting for the writer.
        pub(crate) fn queued(&self) -> usize {
            self.0.staging().queue.len()
        }

        /// Bytes in the block being filled, if there is one.
        fn filling(&self) -> Option<usize> {
            self.0.staging().filling.as_ref().map(|block| block.used)
        }
    }

    /// `length` bytes that differ from those of other seeds.
    fn packet(seed: u8, length: usize) -> Vec<u8> {
        (0..length)
            .map(|at| {
                seed.wrapping_mul(31)
                    .wrapping_add(u8::try_from(at % 251).unwrap())
            })
            .collect()
    }

    fn put(store: &mut DiskStore, index: u64, data: &[u8]) -> Entry {
        Entry::Stored {
            index,
            keyframe: index == 0,
            slot: store.put(data).unwrap(),
        }
    }

    fn put_all(store: &mut DiskStore, packets: &[Vec<u8>]) -> Vec<Entry> {
        (0..)
            .zip(packets)
            .map(|(index, data)| put(store, index, data))
            .collect()
    }

    fn read_back(entries: &[Entry]) -> Vec<Vec<u8>> {
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
        // Two and a half blocks: two go to the file, and the half stays in staging.
        let first = packet(1, 2 * SECTOR_BYTES + SECTOR_BYTES / 2);
        let mut entries = vec![put(&mut store, 0, &first)];
        store.control().settle();
        assert_eq!(store.control().filling(), Some(SECTOR_BYTES / 2));
        assert_eq!(read_back(&entries), std::slice::from_ref(&first));

        // One that follows it in the block being filled, and one that needs the next page.
        let (second, third) = (packet(2, 100), packet(3, 2 * SECTOR_BYTES));
        entries.push(put(&mut store, 1, &second));
        entries.push(put(&mut store, 2, &third));
        assert_matches!(&entries[2], Entry::Stored { slot, .. } if slot.offset == 0);
        assert_eq!(read_back(&entries), [first, second, third]);
    }

    #[test]
    fn a_page_is_filled_again_once_nothing_holds_it() {
        let folder = tempfile::tempdir().unwrap();
        let mut store = DiskStore::small(folder.path(), 2);
        // With the writer held, the first page's old blocks are still queued when it is
        // filled again: the newer bytes must win.
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
        // Past the file's end, staging alone holds the bytes.
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
            Err(StoreError::Call { .. })
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

        // The next buffer replaces the file while a save may still read the old one.
        let mut next = DiskStore::small(folder.path(), 2);
        let fresh = put(&mut next, 0, &packet(1, 5000));
        assert_eq!(read_back(&[kept]), [packet(0, 5000)]);
        drop(next);
        assert_eq!(read_back(&[fresh]), [packet(1, 5000)]);

        // The writer lets go of the file soon after its store has gone.
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
