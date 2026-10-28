//! The rolling packet buffer, trimmed a whole group of pictures at a time so a save starts on
//! a keyframe.

use super::store::{Data, Entry, Store, StoreError};
use std::{collections::VecDeque, mem, sync::Arc};

pub(super) struct Ring {
    packets: VecDeque<Arc<Entry>>,
    keyframes: VecDeque<u64>,
    /// Frames of history to keep.
    span: u64,
    bytes: usize,
    store: Option<Box<dyn Store>>,
    /// Whether the store refused the latest packet it was offered.
    behind: bool,
}

/// A change in where new packets are kept.
#[derive(Debug)]
pub(super) enum StoreChange {
    /// The store refused a packet for now. It stays in RAM, as does every later packet the store
    /// refuses.
    FellBehind(StoreError),
    /// The store takes packets again.
    CaughtUp,
    /// The store failed and is given up, so this and every later packet stay in RAM.
    Failed(StoreError),
}

impl Ring {
    pub(super) fn new(span: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            keyframes: VecDeque::new(),
            span: span.max(1),
            bytes: 0,
            store: None,
            behind: false,
        }
    }

    pub(super) fn set_store(&mut self, store: Box<dyn Store>) {
        self.store = Some(store);
        self.behind = false;
    }

    /// Pushes a packet, and returns how its placement changed where packets are kept.
    pub(super) fn push(&mut self, mut entry: Entry) -> Option<StoreChange> {
        if self.packets.is_empty() && !entry.keyframe {
            return None;
        }
        let change = self.place(&mut entry);
        if entry.keyframe {
            self.keyframes.push_back(entry.index);
        }
        self.bytes += entry.length();
        let latest = entry.index;
        self.packets.push_back(Arc::new(entry));
        self.drop_covered_groups(latest);
        change
    }

    fn place(&mut self, entry: &mut Entry) -> Option<StoreChange> {
        let store = self.store.as_mut()?;
        let Data::Memory(data) = &entry.data else {
            return None;
        };
        match store.put(data) {
            Ok(slot) => {
                entry.data = Data::Stored(slot);
                mem::take(&mut self.behind).then_some(StoreChange::CaughtUp)
            }
            Err(error @ StoreError::Refused(_)) => {
                (!mem::replace(&mut self.behind, true)).then_some(StoreChange::FellBehind(error))
            }
            Err(error) => {
                self.store = None;
                self.behind = false;
                Some(StoreChange::Failed(error))
            }
        }
    }

    fn drop_covered_groups(&mut self, latest: u64) {
        while let Some(&next) = self.keyframes.get(1) {
            // Packet indices rise, so no kept keyframe is past the latest packet. One that is
            // would be a caller's bug, and the saturating span then keeps the group.
            debug_assert!(latest >= next, "packet {latest} came after keyframe {next}");
            if latest.saturating_sub(next) + 1 < self.span {
                break;
            }
            while let Some(dropped) = self.packets.pop_front_if(|entry| entry.index < next) {
                self.bytes -= dropped.length();
            }
            self.keyframes.pop_front();
        }
    }

    pub(super) fn first_index(&self) -> Option<u64> {
        self.packets.front().map(|entry| entry.index)
    }

    pub(super) fn snapshot(&self) -> Vec<Arc<Entry>> {
        self.packets.iter().cloned().collect()
    }

    pub(super) fn frames(&self) -> u64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(first), Some(last)) => last.index - first.index + 1,
            _ => 0,
        }
    }

    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whether the newest packet was placed in the store outside RAM.
    pub(super) fn stored(&self) -> bool {
        self.packets
            .back()
            .is_some_and(|entry| matches!(entry.data, Data::Stored(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::{super::store::tests::MemoryStore, *};
    use crate::replay::{Packet, store::fetch};
    use std::assert_matches;

    fn packet(index: u64, gop: u64) -> Entry {
        Packet {
            index,
            keyframe: index.is_multiple_of(gop),
            data: vec![0; 10].into(),
        }
        .into()
    }

    #[test]
    fn keeps_at_least_the_span_starting_on_a_keyframe() {
        let mut ring = Ring::new(10);
        for index in 0..100 {
            assert!(ring.push(packet(index, 4)).is_none());
            let snapshot = ring.snapshot();
            assert!(snapshot[0].keyframe);
            assert!(ring.frames() >= (index + 1).min(10));
            assert!(ring.frames() < 10 + 4);
            assert_eq!(ring.bytes(), snapshot.len() * 10);
        }
        assert_eq!(ring.snapshot().last().unwrap().index, 99);
    }

    #[test]
    fn waits_for_a_keyframe_before_buffering() {
        let mut ring = Ring::new(10);
        assert!(ring.push(packet(1, 4)).is_none());
        assert!(ring.push(packet(2, 4)).is_none());
        assert_eq!(ring.frames(), 0);

        assert!(ring.push(packet(4, 4)).is_none());
        assert!(ring.push(packet(5, 4)).is_none());
        assert_eq!(ring.frames(), 2);
        assert_eq!(ring.snapshot()[0].index, 4);
    }

    #[test]
    fn skipped_frame_indices_count_towards_the_span() {
        let mut ring = Ring::new(10);
        for index in [0, 1, 2, 3, 20, 21, 22, 23, 24] {
            assert!(ring.push(packet(index, 4)).is_none());
        }
        assert_eq!(ring.snapshot()[0].index, 0);

        assert!(ring.push(packet(28, 4)).is_none());
        assert!(ring.push(packet(29, 4)).is_none());
        assert_eq!(ring.snapshot()[0].index, 20);
    }

    #[test]
    fn stored_packets_are_released_as_they_drop_and_read_back_as_pushed() {
        let mut ring = Ring::new(10);
        ring.set_store(Box::new(MemoryStore::new(25, 7)));
        for index in 0..40 {
            let mut packet = packet(index, 4);
            packet.data = Data::Memory(vec![u8::try_from(index).unwrap(); 10].into());
            assert!(ring.push(packet).is_none());
        }
        let snapshot = ring.snapshot();
        assert!(
            snapshot
                .iter()
                .all(|entry| matches!(entry.data, Data::Stored(_)))
        );
        assert_eq!(ring.bytes(), snapshot.len() * 10);

        let packets = fetch(&snapshot).unwrap();
        for packet in &packets {
            assert_eq!(packet.data[..], [u8::try_from(packet.index).unwrap(); 10]);
            assert_eq!(packet.keyframe, packet.index % 4 == 0);
        }
        assert_eq!(packets.last().unwrap().index, 39);
        assert!(packets[0].keyframe);
    }

    fn in_memory(ring: &Ring) -> Vec<bool> {
        ring.snapshot()
            .iter()
            .map(|entry| matches!(entry.data, Data::Memory(_)))
            .collect()
    }

    #[test]
    fn refused_packets_stay_in_memory_until_the_store_takes_them_again() {
        let mut ring = Ring::new(10);
        let mut store = MemoryStore::new(100, 7);
        store.refused = 2..4;
        ring.set_store(Box::new(store));
        assert!(ring.push(packet(0, 4)).is_none());
        assert!(ring.push(packet(1, 4)).is_none());
        assert_matches!(ring.push(packet(2, 4)), Some(StoreChange::FellBehind(_)));
        assert!(ring.push(packet(3, 4)).is_none(), "reported once");
        assert_matches!(ring.push(packet(4, 4)), Some(StoreChange::CaughtUp));
        assert!(ring.push(packet(5, 4)).is_none());

        assert_eq!(in_memory(&ring), [false, false, true, true, false, false]);
        assert_eq!(fetch(&ring.snapshot()).unwrap().len(), 6);
        assert!(ring.store.is_some());
    }

    #[test]
    fn a_store_that_fails_is_given_up() {
        let mut ring = Ring::new(10);
        ring.set_store(Box::new(MemoryStore::new(10, 7)));
        assert!(ring.push(packet(0, 4)).is_none());
        let mut large = packet(1, 4);
        large.data = Data::Memory(vec![0; 11].into());
        assert_matches!(
            ring.push(large),
            Some(StoreChange::Failed(StoreError::TooLarge(11)))
        );
        assert!(ring.push(packet(2, 4)).is_none());

        assert_eq!(in_memory(&ring), [false, true, true]);
        assert_eq!(fetch(&ring.snapshot()).unwrap().len(), 3);
        assert!(ring.store.is_none());
    }
}
