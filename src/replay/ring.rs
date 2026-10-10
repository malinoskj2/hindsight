//! The rolling buffer: encoded packets for the last stretch of gameplay, trimmed from the front a
//! whole group of pictures at a time so that whatever is saved starts on a keyframe.

use super::Packet;
use std::{collections::VecDeque, sync::Arc};

pub(crate) struct Ring {
    packets: VecDeque<Arc<Packet>>,
    /// Frame indices of the keyframes still in `packets`, oldest first.
    keyframes: VecDeque<u64>,
    /// Frames of history to keep, at least.
    span: u64,
    bytes: usize,
}

impl Ring {
    pub(crate) fn new(span: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            keyframes: VecDeque::new(),
            span: span.max(1),
            bytes: 0,
        }
    }

    /// Appends a packet, then drops the oldest group of pictures while the remaining ones still
    /// cover the span. A buffer never starts with a packet that depends on earlier ones.
    pub(crate) fn push(&mut self, packet: Packet) {
        if self.packets.is_empty() && !packet.keyframe {
            return;
        }
        if packet.keyframe {
            self.keyframes.push_back(packet.index);
        }
        self.bytes += packet.data.len();
        let latest = packet.index;
        self.packets.push_back(Arc::new(packet));
        self.drop_covered_groups(latest);
    }

    /// Drops the oldest group of pictures while the groups after it still cover the span up to
    /// frame `latest`.
    fn drop_covered_groups(&mut self, latest: u64) {
        while let Some(&next) = self.keyframes.get(1)
            && latest - next + 1 >= self.span
        {
            while let Some(dropped) = self.packets.pop_front_if(|packet| packet.index < next) {
                self.bytes -= dropped.data.len();
            }
            self.keyframes.pop_front();
        }
    }

    /// The buffered packets, oldest first; cheap, since packets are shared.
    pub(crate) fn snapshot(&self) -> Vec<Arc<Packet>> {
        self.packets.iter().cloned().collect()
    }

    /// Frames between the first buffered packet and the newest, inclusive.
    #[cfg(test)]
    pub(crate) fn frames(&self) -> u64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(first), Some(last)) => last.index - first.index + 1,
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
    use super::*;

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
            ring.push(packet(index, 4));
            let snapshot = ring.snapshot();
            assert!(snapshot[0].keyframe);
            assert!(ring.frames() >= (index + 1).min(10));
            // Never more than one extra group of pictures beyond the span.
            assert!(ring.frames() < 10 + 4);
            assert_eq!(ring.bytes(), snapshot.len() * 10);
        }
        assert_eq!(ring.snapshot().last().unwrap().index, 99);
    }

    #[test]
    fn waits_for_a_keyframe_before_buffering() {
        let mut ring = Ring::new(10);
        ring.push(packet(1, 4));
        ring.push(packet(2, 4));
        assert_eq!(ring.frames(), 0);

        ring.push(packet(4, 4));
        ring.push(packet(5, 4));
        assert_eq!(ring.frames(), 2);
        assert_eq!(ring.snapshot()[0].index, 4);
    }

    #[test]
    fn skipped_frame_indices_count_towards_the_span() {
        let mut ring = Ring::new(10);
        for index in [0, 1, 2, 3, 20, 21, 22, 23, 24] {
            ring.push(packet(index, 4));
        }
        // Frame 20 starts a group that alone covers 5 frames; the earlier group is still needed.
        assert_eq!(ring.snapshot()[0].index, 0);

        ring.push(packet(28, 4));
        ring.push(packet(29, 4));
        assert_eq!(ring.snapshot()[0].index, 20);
    }
}
