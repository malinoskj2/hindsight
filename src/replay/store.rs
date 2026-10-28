//! Stores that keep the buffer's packets outside system memory, in pages that their slots keep
//! alive.

use super::Packet;
use std::{collections::VecDeque, error::Error, fmt, iter::Peekable, ptr, sync::Arc};
use thiserror::Error;

pub(crate) const PAGE_BYTES: usize = 64 << 20;
/// Bytes a save reads back at a time.
pub(crate) const CHUNK_BYTES: usize = 32 << 20;
pub(crate) const MEBIBYTE: usize = 1 << 20;

/// Returns the pages for `bytes` of packets, plus one for the group of pictures kept beyond the
/// span.
pub(crate) fn reserved_pages(bytes: u64) -> usize {
    usize::try_from(bytes)
        .unwrap_or(usize::MAX)
        .div_ceil(PAGE_BYTES)
        .saturating_add(1)
}

/// Returns the pages for `bytes` of packets, or an error when they would not fit in `available`.
pub(crate) fn pages_within(bytes: u64, available: usize) -> Result<usize, BufferTooLarge> {
    let pages = reserved_pages(bytes);
    let needed = pages.saturating_mul(PAGE_BYTES);
    if needed > available {
        return Err(BufferTooLarge { needed, available });
    }
    Ok(pages)
}

#[derive(Debug, Error)]
#[error("the buffer would take {} MiB and the GPU has {} MiB", needed / MEBIBYTE, available / MEBIBYTE)]
pub(crate) struct BufferTooLarge {
    needed: usize,
    available: usize,
}

/// Returns the offsets and lengths that cover `length` bytes `chunk` at a time.
pub(crate) fn pieces(length: usize, chunk: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..length)
        .step_by(chunk.max(1))
        .map(move |done| (done, chunk.min(length - done)))
}

/// The reason the store failed. The message names only what failed, and the cause is its
/// `source()`.
#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("a {0} byte packet is larger than a page of the store")]
    TooLarge(usize),
    /// The store has no room for the packet right now. The reason is intended for the log.
    #[error("{0}")]
    Refused(String),
    #[error("{step}")]
    Call {
        step: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl StoreError {
    pub(crate) fn call<E: Error + Send + Sync + 'static>(
        step: &'static str,
    ) -> impl FnOnce(E) -> Self {
        move |source| Self::Call {
            step,
            source: Box::new(source),
        }
    }
}

pub(crate) trait Page: Send + Sync {
    /// Passes the range to `sink` in pieces, in order, each with its offset from `offset`.
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError>;
}

/// A range of a page. A clone keeps the page alive.
#[derive(Clone)]
pub(crate) struct Slot {
    page: Arc<dyn Page>,
    offset: usize,
    length: usize,
}

impl fmt::Debug for Slot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Slot")
            .field("page", &Arc::as_ptr(&self.page).cast::<u8>())
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish()
    }
}

impl Slot {
    pub(crate) fn new(page: Arc<dyn Page>, offset: usize, length: usize) -> Self {
        Self {
            page,
            offset,
            length,
        }
    }

    #[cfg(test)]
    pub(super) fn offset(&self) -> usize {
        self.offset
    }

    fn shares_page(&self, other: &Self) -> bool {
        ptr::addr_eq(Arc::as_ptr(&self.page), Arc::as_ptr(&other.page))
    }
}

pub(crate) trait Store: Send {
    fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError>;
}

pub(crate) struct Pages<P> {
    used: VecDeque<Arc<P>>,
    free: Vec<Arc<P>>,
    /// Bytes written to the newest page in use.
    fill: usize,
    page_bytes: usize,
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows GPU stores use it")
    )]
    reserved: usize,
}

impl<P: Page + 'static> Pages<P> {
    pub(crate) fn new(page_bytes: usize, reserved: Vec<P>) -> Self {
        Self {
            used: VecDeque::new(),
            reserved: reserved.len(),
            free: reserved.into_iter().map(Arc::new).collect(),
            fill: 0,
            page_bytes,
        }
    }

    /// Creates `count` pages up front, so recording never waits on one.
    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows GPU stores use it")
    )]
    pub(crate) fn reserve<E>(
        page_bytes: usize,
        count: usize,
        make: impl FnMut() -> Result<P, E>,
    ) -> Result<Self, E> {
        let reserved = std::iter::repeat_with(make)
            .take(count)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(page_bytes, reserved))
    }

    #[cfg_attr(
        all(not(windows), test),
        expect(dead_code, reason = "only the Windows GPU stores use it")
    )]
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} MiB in {} MiB pages",
            self.reserved * self.page_bytes / MEBIBYTE,
            self.page_bytes / MEBIBYTE
        )
    }

    /// Returns a page and offset for `length` bytes. A packet never straddles two pages.
    pub(crate) fn place(
        &mut self,
        length: usize,
        make: impl FnOnce() -> Result<P, StoreError>,
    ) -> Result<(Arc<P>, usize), StoreError> {
        if length > self.page_bytes {
            return Err(StoreError::TooLarge(length));
        }
        let page = match self.used.back() {
            Some(page) if self.fill + length <= self.page_bytes => Arc::clone(page),
            _ => {
                self.reclaim();
                let page = match self.free.pop() {
                    Some(page) => page,
                    None => Arc::new(make()?),
                };
                self.used.push_back(Arc::clone(&page));
                self.fill = 0;
                page
            }
        };
        let offset = self.fill;
        self.fill += length;
        Ok((page, offset))
    }

    /// Frees unheld pages from the front. The page being filled stays in use even when empty.
    fn reclaim(&mut self) {
        while self.used.len() > 1
            && let Some(page) = self.used.pop_front_if(|page| Arc::strong_count(page) == 1)
        {
            self.free.push(page);
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.used.len() + self.free.len()
    }
}

/// A buffered packet: its bytes in memory, or the slot of a store holding them.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) index: u64,
    pub(crate) keyframe: bool,
    pub(crate) data: Data,
}

#[derive(Debug)]
pub(crate) enum Data {
    Memory(Box<[u8]>),
    Stored(Slot),
}

impl Entry {
    pub(crate) fn length(&self) -> usize {
        match &self.data {
            Data::Memory(data) => data.len(),
            Data::Stored(slot) => slot.length,
        }
    }
}

impl From<Packet> for Entry {
    fn from(packet: Packet) -> Self {
        Self {
            index: packet.index,
            keyframe: packet.keyframe,
            data: Data::Memory(packet.data),
        }
    }
}

type Visit<'a, E> = dyn FnMut(&Entry, &[u8]) -> Result<(), E> + 'a;

/// Visits each entry with its bytes, reading each run of neighbours in one page in a single
/// read.
pub(crate) fn each<E: From<StoreError>>(
    entries: &[Arc<Entry>],
    visit: &mut Visit<'_, E>,
) -> Result<(), E> {
    let mut rest = entries;
    while let Some((first, _)) = rest.split_first() {
        match &first.data {
            Data::Memory(data) => {
                visit(first, data)?;
                rest = &rest[1..];
            }
            Data::Stored(slot) => {
                let (count, end) = extent(rest, slot);
                let reader = RunReader {
                    run: run(rest, slot).peekable(),
                    start: slot.offset,
                    straddling: Vec::new(),
                };
                reader.read(&*slot.page, end - slot.offset, visit)?;
                rest = &rest[count..];
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fetch(entries: &[Arc<Entry>]) -> Result<Vec<Arc<Packet>>, StoreError> {
    let mut packets = Vec::with_capacity(entries.len());
    each(entries, &mut |entry, data| {
        packets.push(Arc::new(Packet {
            index: entry.index,
            keyframe: entry.keyframe,
            data: data.into(),
        }));
        Ok::<_, StoreError>(())
    })?;
    Ok(packets)
}

/// Returns the leading entries of `rest` in the page of `first` at rising, non-overlapping
/// offsets, each with its slot.
fn run<'a>(rest: &'a [Arc<Entry>], first: &'a Slot) -> impl Iterator<Item = (&'a Entry, &'a Slot)> {
    let mut end = first.offset;
    rest.iter().map_while(move |entry| match &entry.data {
        Data::Stored(slot) if slot.shares_page(first) && slot.offset >= end => {
            end = slot.offset + slot.length;
            Some((&**entry, slot))
        }
        _ => None,
    })
}

/// Returns the number of entries in the run starting at `first`, and the page offset it ends at.
fn extent(rest: &[Arc<Entry>], first: &Slot) -> (usize, usize) {
    run(rest, first).fold((0, first.offset), |(count, _), (_, last)| {
        (count + 1, last.offset + last.length)
    })
}

/// Maps the pieces of one page read onto the run's entries in order.
struct RunReader<I: Iterator> {
    run: Peekable<I>,
    /// The offset in the page the read starts at.
    start: usize,
    /// A packet split across two pieces, gathered until it is whole.
    straddling: Vec<u8>,
}

impl<'a, I: Iterator<Item = (&'a Entry, &'a Slot)>> RunReader<I> {
    fn read<E: From<StoreError>>(
        mut self,
        page: &dyn Page,
        span: usize,
        visit: &mut Visit<'_, E>,
    ) -> Result<(), E> {
        // The sink returns nothing, so a `visit` error is held until the call returns. Reading a
        // chunk per call lets the read stop there.
        let mut failed = None;
        for (at, length) in pieces(span, CHUNK_BYTES) {
            page.read(self.start + at, length, &mut |offset, piece| {
                if failed.is_none()
                    && let Err(error) = self.piece(at + offset, piece, visit)
                {
                    failed = Some(error);
                }
            })?;
            if let Some(error) = failed {
                return Err(error);
            }
        }
        // Empty packets at the end of the span lie in no piece.
        for (entry, _) in self.run {
            visit(entry, &[])?;
        }
        Ok(())
    }

    fn piece<E>(&mut self, at: usize, piece: &[u8], visit: &mut Visit<'_, E>) -> Result<(), E> {
        let piece_end = at + piece.len();
        while let Some(&(entry, slot)) = self.run.peek() {
            let slot_start = slot.offset - self.start;
            let slot_end = slot_start + slot.length;
            if slot_start >= piece_end {
                break;
            }
            let (from, to) = (slot_start.max(at), slot_end.min(piece_end));
            let bytes = &piece[from - at..to - at];
            if from == slot_start && to == slot_end {
                visit(entry, bytes)?;
            } else {
                if from == slot_start {
                    self.straddling.clear();
                }
                self.straddling.extend_from_slice(bytes);
                if to < slot_end {
                    break;
                }
                visit(entry, &self.straddling)?;
            }
            self.run.next();
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        assert_matches,
        ops::Range,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[test]
    fn pieces_cover_the_length_a_chunk_at_a_time() {
        assert_eq!(pieces(10, 4).collect::<Vec<_>>(), [(0, 4), (4, 4), (8, 2)]);
        assert_eq!(pieces(4, 4).collect::<Vec<_>>(), [(0, 4)]);
        assert_eq!(pieces(0, 4).count(), 0);
        assert_eq!(pieces(0, 0).count(), 0);
    }

    #[test]
    fn the_reservation_is_refused_when_the_pages_would_not_fit() {
        assert_eq!(pages_within(PAGE_BYTES as u64, 3 * PAGE_BYTES).unwrap(), 2);
        let refused = pages_within(PAGE_BYTES as u64, PAGE_BYTES).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "the buffer would take 128 MiB and the GPU has 64 MiB"
        );
    }

    pub(crate) struct MemoryPage {
        bytes: Mutex<Vec<u8>>,
        piece: usize,
        reads: Arc<AtomicUsize>,
    }

    impl Page for MemoryPage {
        fn read(
            &self,
            offset: usize,
            length: usize,
            sink: &mut dyn FnMut(usize, &[u8]),
        ) -> Result<(), StoreError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let bytes = self.bytes.lock().unwrap();
            for (at, piece) in bytes[offset..offset + length]
                .chunks(self.piece)
                .enumerate()
                .map(|(index, piece)| (index * self.piece, piece))
            {
                sink(at, piece);
            }
            Ok(())
        }
    }

    pub(crate) struct MemoryStore {
        pages: Pages<MemoryPage>,
        page_bytes: usize,
        piece: usize,
        /// The attempts to put a packet, counted from 0, that are refused.
        pub(crate) refused: Range<usize>,
        attempts: usize,
        reads: Arc<AtomicUsize>,
    }

    impl MemoryStore {
        pub(crate) fn new(page_bytes: usize, piece: usize) -> Self {
            Self {
                pages: Pages::new(page_bytes, Vec::new()),
                page_bytes,
                piece,
                refused: 0..0,
                attempts: 0,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub(crate) fn pages(&self) -> usize {
            self.pages.count()
        }

        /// Calls to `Page::read` on any of the pages.
        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
    }

    impl Store for MemoryStore {
        fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError> {
            let attempt = self.attempts;
            self.attempts += 1;
            if self.refused.contains(&attempt) {
                return Err(StoreError::Refused("the store is full".to_owned()));
            }
            let (page_bytes, piece) = (self.page_bytes, self.piece);
            let reads = Arc::clone(&self.reads);
            let (page, offset) = self.pages.place(data.len(), || {
                Ok(MemoryPage {
                    bytes: Mutex::new(vec![0; page_bytes]),
                    piece,
                    reads,
                })
            })?;
            page.bytes.lock().unwrap()[offset..offset + data.len()].copy_from_slice(data);
            Ok(Slot::new(page, offset, data.len()))
        }
    }

    fn stored(store: &mut MemoryStore, index: u64, data: &[u8]) -> Arc<Entry> {
        Arc::new(Entry {
            index,
            keyframe: index == 0,
            data: Data::Stored(store.put(data).unwrap()),
        })
    }

    #[test]
    fn packets_come_back_from_pages_in_pieces_and_from_memory() {
        let mut store = MemoryStore::new(10, 3);
        let in_memory = Arc::new(Entry {
            index: 2,
            keyframe: false,
            data: Data::Memory(vec![9; 4].into()),
        });
        let entries = [
            stored(&mut store, 0, &[1, 2, 3, 4]),
            stored(&mut store, 1, &[5, 6, 7, 8, 9]),
            in_memory,
            stored(&mut store, 3, &[10, 11]),
            stored(&mut store, 4, &[12]),
        ];
        assert_eq!(store.pages(), 2);

        let packets = fetch(&entries).unwrap();
        let data: Vec<&[u8]> = packets.iter().map(|packet| &packet.data[..]).collect();
        assert_eq!(
            data,
            [
                &[1, 2, 3, 4][..],
                &[5, 6, 7, 8, 9],
                &[9, 9, 9, 9],
                &[10, 11],
                &[12]
            ]
        );
        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.index)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(packets[0].keyframe && !packets[1].keyframe);
    }

    #[test]
    fn a_run_ends_where_the_offsets_stop_rising() {
        let page: Arc<dyn Page> = Arc::new(MemoryPage {
            bytes: Mutex::new((0..10).collect()),
            piece: 10,
            reads: Arc::default(),
        });
        let entry = |index, offset, length| {
            Arc::new(Entry {
                index,
                keyframe: false,
                data: Data::Stored(Slot::new(Arc::clone(&page), offset, length)),
            })
        };
        let entries = [
            entry(0, 6, 2),
            entry(1, 2, 3),
            entry(2, 4, 2),
            entry(3, 6, 1),
        ];

        let packets = fetch(&entries).unwrap();
        let data: Vec<&[u8]> = packets.iter().map(|packet| &packet.data[..]).collect();
        assert_eq!(data, [&[6, 7][..], &[2, 3, 4], &[4, 5], &[6]]);
        assert_eq!(
            run(&entries, &Slot::new(Arc::clone(&page), 6, 2)).count(),
            1
        );
        assert_eq!(run(&entries[1..], &Slot::new(page, 2, 3)).count(), 1);
    }

    #[test]
    fn a_run_is_read_a_chunk_at_a_time_and_no_further_than_a_failed_visit() {
        let mut store = MemoryStore::new(2 * CHUNK_BYTES, CHUNK_BYTES);
        let entries = [
            stored(&mut store, 0, &vec![1; CHUNK_BYTES]),
            stored(&mut store, 1, &[2]),
        ];
        assert_eq!(fetch(&entries).unwrap().len(), 2);
        assert_eq!(store.reads(), 2);

        let refused = each(&entries, &mut |_, _| {
            Err(StoreError::Refused("the file is full".to_owned()))
        });
        assert_matches!(refused, Err(StoreError::Refused(_)));
        assert_eq!(store.reads(), 3);
    }

    #[test]
    fn pages_are_filled_again_once_nothing_holds_their_slots() {
        let mut store = MemoryStore::new(10, 10);
        let first = store.put(&[1; 6]).unwrap();
        let second = store.put(&[2; 6]).unwrap();
        assert_eq!(store.pages(), 2);
        drop(first);
        let third = store.put(&[3; 6]).unwrap();
        assert_eq!(store.pages(), 2, "the first page was filled again");
        assert_eq!(third.offset, 0);
        drop(second);
        let held = third.clone();
        let fourth = store.put(&[4; 6]).unwrap();
        assert_eq!(store.pages(), 2);
        assert!(!fourth.shares_page(&held));
        let fifth = store.put(&[5; 6]).unwrap();
        assert_eq!(store.pages(), 3, "both pages are held, so a third is made");
        drop((held, third, fourth, fifth));
    }

    #[test]
    fn a_buffer_reserves_its_pages_and_one_more() {
        let page = PAGE_BYTES as u64;
        assert_eq!(reserved_pages(0), 1);
        assert_eq!(reserved_pages(1), 2);
        assert_eq!(reserved_pages(page), 2);
        assert_eq!(reserved_pages(page + 1), 3);
        assert_eq!(reserved_pages(10 * page), 11);
    }

    #[test]
    fn a_store_can_refuse_a_packet() {
        let mut store = MemoryStore::new(4, 2);
        assert_matches!(store.put(&[0; 5]), Err(StoreError::TooLarge(5)));
        store.refused = 0..usize::MAX;
        assert_matches!(store.put(&[0; 1]), Err(StoreError::Refused(_)));
    }
}
