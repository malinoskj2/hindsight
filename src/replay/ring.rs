//! The rolling packet buffer, trimmed a whole group of pictures at a time so a save starts on
//! a keyframe.

use super::{
    Packet,
    store::{Entry, Store, StoreError},
};
use std::{collections::VecDeque, sync::Arc};

pub(crate) struct Ring {
    packets: VecDeque<Entry>,
    keyframes: VecDeque<u64>,
    /// Frames of history to keep.
    span: u64,
    bytes: usize,
    store: Option<Box<dyn Store>>,
}

impl Ring {
    pub(crate) fn new(span: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            keyframes: VecDeque::new(),
            span: span.max(1),
            bytes: 0,
            store: None,
        }
    }

    pub(crate) fn set_store(&mut self, store: Box<dyn Store>) {
        self.store = Some(store);
    }

    /// An error means the store refused the packet; it and every later one stay in memory.
    pub(crate) fn push(&mut self, packet: Packet) -> Result<(), StoreError> {
        if self.packets.is_empty() && !packet.keyframe {
            return Ok(());
        }
        let (entry, refused) = self.place(packet);
        self.push_entry(entry);
        refused.map_or(Ok(()), Err)
    }

    pub(crate) fn push_entry(&mut self, entry: Entry) {
        if self.packets.is_empty() && !entry.keyframe() {
            return;
        }
        if entry.keyframe() {
            self.keyframes.push_back(entry.index());
        }
        self.bytes += entry.length();
        let latest = entry.index();
        self.packets.push_back(entry);
        self.drop_covered_groups(latest);
    }

    fn place(&mut self, packet: Packet) -> (Entry, Option<StoreError>) {
        let Some(store) = self.store.as_mut() else {
            return (Entry::Memory(Arc::new(packet)), None);
        };
        match store.put(&packet.data) {
            Ok(slot) => (
                Entry::Stored {
                    index: packet.index,
                    keyframe: packet.keyframe,
                    slot,
                },
                None,
            ),
            Err(error) => {
                self.store = None;
                (Entry::Memory(Arc::new(packet)), Some(error))
            }
        }
    }

    fn drop_covered_groups(&mut self, latest: u64) {
        while let Some(&next) = self.keyframes.get(1)
            && latest - next + 1 >= self.span
        {
            while let Some(dropped) = self.packets.pop_front_if(|entry| entry.index() < next) {
                self.bytes -= dropped.length();
            }
            self.keyframes.pop_front();
        }
    }

    pub(crate) fn first_index(&self) -> Option<u64> {
        self.packets.front().map(Entry::index)
    }

    pub(crate) fn snapshot(&self) -> Vec<Entry> {
        self.packets.iter().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn frames(&self) -> u64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(first), Some(last)) => last.index() - first.index() + 1,
            _ => 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{super::store::tests::MemoryStore, *};
    use crate::replay::store::fetch;

    fn packet(index: u64, gop: u64) -> Packet {
        Packet {
            index,
            keyframe: index.is_multiple_of(gop),
            data: vec![0; 10].into(),
        }
    }

    #[test]
    fn keeps_at_least_the_span_starting_on_a_keyframe() {
        let mut ring = Ring::new(10);
        for index in 0..100 {
            ring.push(packet(index, 4)).unwrap();
            let snapshot = ring.snapshot();
            assert!(snapshot[0].keyframe());
            assert!(ring.frames() >= (index + 1).min(10));
            assert!(ring.frames() < 10 + 4);
            assert_eq!(ring.bytes(), snapshot.len() * 10);
        }
        assert_eq!(ring.snapshot().last().unwrap().index(), 99);
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
        assert_eq!(ring.snapshot()[0].index(), 4);
    }

    #[test]
    fn skipped_frame_indices_count_towards_the_span() {
        let mut ring = Ring::new(10);
        for index in [0, 1, 2, 3, 20, 21, 22, 23, 24] {
            ring.push(packet(index, 4)).unwrap();
        }
        assert_eq!(ring.snapshot()[0].index(), 0);

        ring.push(packet(28, 4)).unwrap();
        ring.push(packet(29, 4)).unwrap();
        assert_eq!(ring.snapshot()[0].index(), 20);
    }

    #[test]
    fn stored_packets_are_released_as_they_drop_and_read_back_as_pushed() {
        let mut ring = Ring::new(10);
        ring.set_store(Box::new(MemoryStore::new(25, 7)));
        for index in 0..40 {
            let mut packet = packet(index, 4);
            packet.data = vec![u8::try_from(index).unwrap(); 10].into();
            ring.push(packet).unwrap();
        }
        let snapshot = ring.snapshot();
        assert!(
            snapshot
                .iter()
                .all(|entry| matches!(entry, Entry::Stored { .. }))
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
        assert!(matches!(snapshot[1], Entry::Stored { .. }));
        assert!(matches!(snapshot[2], Entry::Memory(_)));
        assert!(matches!(snapshot[3], Entry::Memory(_)));
        assert_eq!(fetch(&snapshot).unwrap().len(), 4);
        assert!(ring.store.is_none());
    }
}
