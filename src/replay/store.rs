//! Where the buffer's packets may be kept instead of system memory. A store hands out slots in
//! pages of its own (the GPU's memory, say) as packets arrive, and a save reads whole runs of
//! slots back from a page at a time, since reading each packet on its own would cost a round
//! trip per frame.

use super::Packet;
use std::{error::Error, fmt, ptr, sync::Arc};
use thiserror::Error;

/// Why a store could not take a packet or give it back.
#[derive(Debug, Error)]
pub(crate) enum StoreError {
    /// The packet is larger than a page of the store.
    #[error("a {0} byte packet is larger than a page of the store")]
    TooLarge(usize),
    /// A call into the store's memory failed; `step` says which, in words for the log.
    #[error("{step}: {source}")]
    Call {
        step: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl StoreError {
    /// Wraps the error of `step`, for `map_err`.
    pub(crate) fn call<E: Error + Send + Sync + 'static>(
        step: &'static str,
    ) -> impl FnOnce(E) -> Self {
        move |source| Self::Call {
            step,
            source: Box::new(source),
        }
    }
}

/// A page of a store: a block of its memory that slots are carved from in order, and read
/// back in bulk.
pub(crate) trait Page: Send + Sync {
    /// Reads the `length` bytes at `offset`, handing `sink` each piece as it arrives with the
    /// piece's offset from `offset`; the pieces come in order and cover the range exactly.
    fn read(
        &self,
        offset: usize,
        length: usize,
        sink: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError>;
}

/// Where a packet was put: a range of a page. Cloning a slot keeps its page alive, so a save
/// can read it after the store has dropped the packet.
#[derive(Clone)]
pub(crate) struct Slot {
    pub(crate) page: Arc<dyn Page>,
    pub(crate) offset: usize,
    pub(crate) length: usize,
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
    /// Whether both slots lie in the same page.
    fn shares_page(&self, other: &Self) -> bool {
        ptr::addr_eq(Arc::as_ptr(&self.page), Arc::as_ptr(&other.page))
    }
}

/// A store of packets outside system memory.
pub(crate) trait Store: Send {
    /// Puts `data` away and says where.
    fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError>;

    /// Gives `slot` back; its space is reused once nothing reads it any more.
    fn release(&mut self, slot: Slot);
}

/// A buffered packet: held in memory, or put away in a store with only its header kept here.
#[derive(Clone, Debug)]
pub(crate) enum Entry {
    Memory(Arc<Packet>),
    Stored {
        index: u64,
        keyframe: bool,
        slot: Slot,
    },
}

impl Entry {
    /// The packet's frame index.
    pub(crate) fn index(&self) -> u64 {
        match self {
            Self::Memory(packet) => packet.index,
            Self::Stored { index, .. } => *index,
        }
    }

    /// Whether the packet is a keyframe.
    pub(crate) fn keyframe(&self) -> bool {
        match self {
            Self::Memory(packet) => packet.keyframe,
            Self::Stored { keyframe, .. } => *keyframe,
        }
    }

    /// The packet's size in bytes.
    pub(crate) fn length(&self) -> usize {
        match self {
            Self::Memory(packet) => packet.data.len(),
            Self::Stored { slot, .. } => slot.length,
        }
    }
}

/// The packets of `entries`, those put away read back from their pages: each run of
/// neighbours in one page in a single read.
pub(crate) fn fetch(entries: &[Entry]) -> Result<Vec<Arc<Packet>>, StoreError> {
    let mut packets = Vec::with_capacity(entries.len());
    let mut rest = entries;
    while let Some((first, _)) = rest.split_first() {
        match first {
            Entry::Memory(packet) => {
                packets.push(Arc::clone(packet));
                rest = &rest[1..];
            }
            Entry::Stored { slot, .. } => {
                let run = rest
                    .iter()
                    .take_while(|entry| {
                        matches!(entry, Entry::Stored { slot: other, .. } if slot.shares_page(other) && other.offset >= slot.offset)
                    })
                    .count();
                packets.extend(read_run(&rest[..run])?);
                rest = &rest[run..];
            }
        }
    }
    Ok(packets)
}

/// Reads back `run`, stored packets in one page at rising offsets, in one pass over the
/// range they span.
fn read_run(run: &[Entry]) -> Result<Vec<Arc<Packet>>, StoreError> {
    let slots: Vec<&Slot> = run
        .iter()
        .map(|entry| match entry {
            Entry::Stored { slot, .. } => slot,
            Entry::Memory(_) => unreachable!("a run holds stored packets only"),
        })
        .collect();
    let (Some(first), Some(last)) = (slots.first(), slots.last()) else {
        return Ok(Vec::new());
    };
    let start = first.offset;
    let span = last.offset + last.length - start;
    let mut buffers: Vec<Box<[u8]>> = slots
        .iter()
        .map(|slot| vec![0; slot.length].into_boxed_slice())
        .collect();

    // Each piece is spread over the packets it covers, in order.
    let mut next = 0;
    first.page.read(start, span, &mut |at, piece| {
        let piece_end = at + piece.len();
        while let Some(slot) = slots.get(next) {
            let (slot_start, slot_end) = (slot.offset - start, slot.offset - start + slot.length);
            if slot_start >= piece_end {
                break;
            }
            let (from, to) = (slot_start.max(at), slot_end.min(piece_end));
            buffers[next][from - slot_start..to - slot_start]
                .copy_from_slice(&piece[from - at..to - at]);
            if slot_end > piece_end {
                break;
            }
            next += 1;
        }
    })?;

    Ok(run
        .iter()
        .zip(buffers)
        .map(|(entry, data)| {
            Arc::new(Packet {
                index: entry.index(),
                keyframe: entry.keyframe(),
                data,
            })
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{assert_matches, sync::Mutex};

    /// A page in memory, read back in pieces of a few bytes to exercise the spreading.
    pub(crate) struct MemoryPage {
        bytes: Mutex<Vec<u8>>,
        piece: usize,
    }

    impl Page for MemoryPage {
        fn read(
            &self,
            offset: usize,
            length: usize,
            sink: &mut dyn FnMut(usize, &[u8]),
        ) -> Result<(), StoreError> {
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

    /// A store of small pages in memory: packets go in one after another, a page at a time,
    /// and the pages are reused once released. `fail_after` packets, it refuses.
    pub(crate) struct MemoryStore {
        pub(crate) page_bytes: usize,
        pub(crate) piece: usize,
        pub(crate) fail_after: usize,
        pages: Vec<Arc<MemoryPage>>,
        fill: usize,
        put: usize,
        pub(crate) released: usize,
    }

    impl MemoryStore {
        pub(crate) fn new(page_bytes: usize, piece: usize) -> Self {
            Self {
                page_bytes,
                piece,
                fail_after: usize::MAX,
                pages: Vec::new(),
                fill: 0,
                put: 0,
                released: 0,
            }
        }

        /// Pages the store has created.
        pub(crate) fn pages(&self) -> usize {
            self.pages.len()
        }
    }

    impl Store for MemoryStore {
        fn put(&mut self, data: &[u8]) -> Result<Slot, StoreError> {
            if self.put >= self.fail_after {
                return Err(StoreError::call("store the packet")(std::io::Error::other(
                    "the store is full",
                )));
            }
            if data.len() > self.page_bytes {
                return Err(StoreError::TooLarge(data.len()));
            }
            if self.pages.is_empty() || self.fill + data.len() > self.page_bytes {
                self.pages.push(Arc::new(MemoryPage {
                    bytes: Mutex::new(vec![0; self.page_bytes]),
                    piece: self.piece,
                }));
                self.fill = 0;
            }
            let page = self.pages.last().unwrap();
            page.bytes.lock().unwrap()[self.fill..self.fill + data.len()].copy_from_slice(data);
            let slot = Slot {
                page: Arc::clone(page) as Arc<dyn Page>,
                offset: self.fill,
                length: data.len(),
            };
            self.fill += data.len();
            self.put += 1;
            Ok(slot)
        }

        fn release(&mut self, _slot: Slot) {
            self.released += 1;
        }
    }

    fn stored(store: &mut MemoryStore, index: u64, data: &[u8]) -> Entry {
        Entry::Stored {
            index,
            keyframe: index == 0,
            slot: store.put(data).unwrap(),
        }
    }

    #[test]
    fn packets_come_back_from_pages_in_pieces_and_from_memory_as_they_are() {
        let mut store = MemoryStore::new(10, 3);
        let in_memory = Arc::new(Packet {
            index: 2,
            keyframe: false,
            data: vec![9; 4].into(),
        });
        let entries = [
            stored(&mut store, 0, &[1, 2, 3, 4]),
            stored(&mut store, 1, &[5, 6, 7, 8, 9]),
            Entry::Memory(Arc::clone(&in_memory)),
            // Does not fit the first page's last byte: a new page.
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
        assert!(Arc::ptr_eq(&packets[2], &in_memory));
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
    fn a_store_can_refuse_a_packet() {
        let mut store = MemoryStore::new(4, 2);
        assert_matches!(store.put(&[0; 5]), Err(StoreError::TooLarge(5)));
        store.fail_after = 0;
        assert_matches!(store.put(&[0; 1]), Err(StoreError::Call { .. }));
    }
}
