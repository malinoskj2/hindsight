//! The rolling packet buffer, trimmed a whole group of pictures at a time so a save starts on
//! a keyframe.

use super::store::{Data, Entry, Store, StoreError};
use std::{collections::VecDeque, sync::Arc};

pub(super) struct Ring {
    packets: VecDeque<Arc<Entry>>,
    keyframes: VecDeque<u64>,
    /// Frames of history to keep.
    span: u64,
    bytes: usize,
    store: Option<Box<dyn Store>>,
}

impl Ring {
    pub(super) fn new(span: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            keyframes: VecDeque::new(),
            span: span.max(1),
            bytes: 0,
            store: None,
        }
    }

    pub(super) fn set_store(&mut self, store: Box<dyn Store>) {
        self.store = Some(store);
    }

    /// An error means the store refused the packet; it and every later one stay in memory.
    pub(super) fn push(&mut self, mut entry: Entry) -> Result<(), StoreError> {
        if self.packets.is_empty() && !entry.keyframe {
            return Ok(());
        }
        let refused = self.place(&mut entry);
        if entry.keyframe {
            self.keyframes.push_back(entry.index);
        }
        self.bytes += entry.length();
        let latest = entry.index;
        self.packets.push_back(Arc::new(entry));
        self.drop_covered_groups(latest);
        refused.map_or(Ok(()), Err)
    }

    fn place(&mut self, entry: &mut Entry) -> Option<StoreError> {
        let store = self.store.as_mut()?;
        let Data::Memory(data) = &entry.data else {
            return None;
        };
        match store.put(data) {
            Ok(slot) => {
                entry.data = Data::Stored(slot);
                None
            }
            Err(error) => {
                self.store = None;
                Some(error)
            }
        }
    }

    fn drop_covered_groups(&mut self, latest: u64) {
        while let Some(&next) = self.keyframes.get(1)
            && latest - next + 1 >= self.span
        {
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

    /// Whether the newest packet went to the store outside RAM.
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
            ring.push(packet(index, 4)).unwrap();
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
        ring.push(packet(1, 4)).unwrap();
        ring.push(packet(2, 4)).unwrap();
        assert_eq!(ring.frames(), 0);

        ring.push(packet(4, 4)).unwrap();
        ring.push(packet(5, 4)).unwrap();
        assert_eq!(ring.frames(), 2);
        assert_eq!(ring.snapshot()[0].index, 4);
    }

    #[test]
    fn skipped_frame_indices_count_towards_the_span() {
        let mut ring = Ring::new(10);
        for index in [0, 1, 2, 3, 20, 21, 22, 23, 24] {
            ring.push(packet(index, 4)).unwrap();
        }
        assert_eq!(ring.snapshot()[0].index, 0);

        ring.push(packet(28, 4)).unwrap();
        ring.push(packet(29, 4)).unwrap();
        assert_eq!(ring.snapshot()[0].index, 20);
    }

    #[test]
    fn stored_packets_are_released_as_they_drop_and_read_back_as_pushed() {
        let mut ring = Ring::new(10);
        ring.set_store(Box::new(MemoryStore::new(25, 7)));
        for index in 0..40 {
            let mut packet = packet(index, 4);
            packet.data = Data::Memory(vec![u8::try_from(index).unwrap(); 10].into());
            ring.push(packet).unwrap();
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

    #[test]
    fn a_refused_packet_stays_in_memory_and_the_store_is_given_up() {
        let mut ring = Ring::new(10);
        let mut store = MemoryStore::new(100, 7);
        store.fail_after = 2;
        ring.set_store(Box::new(store));
        ring.push(packet(0, 4)).unwrap();
        ring.push(packet(1, 4)).unwrap();
        assert!(ring.push(packet(2, 4)).is_err());
        ring.push(packet(3, 4)).unwrap();

        let snapshot = ring.snapshot();
        assert!(matches!(snapshot[1].data, Data::Stored(_)));
        assert!(matches!(snapshot[2].data, Data::Memory(_)));
        assert!(matches!(snapshot[3].data, Data::Memory(_)));
        assert_eq!(fetch(&snapshot).unwrap().len(), 4);
        assert!(ring.store.is_none());
    }
}
