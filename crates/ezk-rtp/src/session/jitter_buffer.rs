use crate::RtpPacket;
use std::{
    cmp,
    collections::{btree_map::Entry, BTreeMap},
};

#[derive(Debug)]
pub(crate) struct JitterBuffer {
    /// maximum number of entries
    max_entries: usize,
    /// sequence-number -> packet map
    entries: BTreeMap<u64, JbEntry>,
    /// highest and lowest sequence number
    state: Option<State>,

    /// num packets dropped
    pub(crate) dropped: u64,

    /// num packets received
    pub(crate) received: u64,
    /// num packets not received
    pub(crate) lost: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            entries: BTreeMap::new(),
            state: None,
            dropped: 0,
            received: 0,
            lost: 0,
        }
    }
}

#[derive(Debug)]
struct State {
    /// highest seq number
    head: u64,
    /// lowest seq number
    tail: u64,

    /// last known timestamp
    last_timestamp: u64,
}

#[derive(Debug)]
struct JbEntry {
    timestamp: u64,
    packet: RtpPacket,
}

impl JitterBuffer {
    pub(crate) fn last_sequence_number(&self) -> Option<u64> {
        self.state.as_ref().map(|s| s.head)
    }

    pub(crate) fn push(&mut self, packet: RtpPacket) {
        let rtp_packet = packet.get();

        let Some(state) = &mut self.state else {
            let sequence_number = u64::from(rtp_packet.sequence_number());
            let timestamp = u64::from(rtp_packet.timestamp());

            self.entries
                .insert(sequence_number, JbEntry { timestamp, packet });

            self.state = Some(State {
                head: sequence_number,
                tail: sequence_number,
                last_timestamp: timestamp,
            });

            return;
        };

        let sequence_number = guess_sequence_number(state.tail, rtp_packet.sequence_number());
        let timestamp = guess_timestamp(state.last_timestamp, rtp_packet.timestamp());
        state.last_timestamp = timestamp;

        if sequence_number < state.tail {
            self.dropped += 1;
            return;
        }

        if let Entry::Vacant(entry) = self.entries.entry(sequence_number) {
            self.received += 1;
            entry.insert(JbEntry { timestamp, packet });
        }

        state.head = cmp::max(state.head, sequence_number);

        self.ensure_max_size();
    }

    fn ensure_max_size(&mut self) {
        if self.entries.len() > self.max_entries {
            let (seq, _) = self.entries.pop_first().unwrap();

            if let Some(state) = &mut self.state {
                state.tail = seq + 1;
            }
        }
    }

    pub(crate) fn pop(&mut self, max_timestamp: u64) -> Option<RtpPacket> {
        let state = self.state.as_mut()?;

        for i in state.tail..=state.head {
            let Entry::Occupied(entry) = self.entries.entry(i) else {
                continue;
            };

            if entry.get().timestamp > max_timestamp {
                return None;
            }

            self.lost += i - state.tail;
            state.tail = i + 1;

            let packet = entry.remove().packet;

            return Some(packet);
        }

        None
    }
}

fn guess_sequence_number(reference: u64, got: u16) -> u64 {
    wrapping_counter_to_u64_counter(reference, u64::from(got), u64::from(u16::MAX))
}

pub(crate) fn guess_timestamp(reference: u64, got: u32) -> u64 {
    wrapping_counter_to_u64_counter(reference, u64::from(got), u64::from(u32::MAX))
}

fn wrapping_counter_to_u64_counter(reference: u64, got: u64, max: u64) -> u64 {
    let mul = (reference / max).saturating_sub(1);

    let low = mul * max + got;
    let high = (mul + 1) * max + got;

    if low.abs_diff(reference) < high.abs_diff(reference) {
        low
    } else {
        high
    }
}

#[cfg(test)]
mod tests {
    use rtp_types::RtpPacketBuilder;

    use super::*;

    fn make_packet(sequence_number: u16, timestamp: u32) -> RtpPacket {
        RtpPacket::new(
            &RtpPacketBuilder::new()
                .sequence_number(sequence_number)
                .timestamp(timestamp),
        )
    }

    #[test]
    fn flimsy_test() {
        let mut jb = JitterBuffer::default();

        jb.push(make_packet(1, 100));
        jb.push(make_packet(4, 400));
        jb.push(make_packet(3, 300));

        assert_eq!(jb.pop(1000).unwrap().get().sequence_number(), 1);
        assert_eq!(jb.pop(1000).unwrap().get().sequence_number(), 3);
        assert_eq!(jb.pop(1000).unwrap().get().sequence_number(), 4);
        assert_eq!(jb.lost, 1)
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn sequence_number_guessing() {
        assert_eq!(guess_sequence_number(0, 0), 0);
        assert_eq!(guess_sequence_number(1, 65535), 65535);
        assert_eq!(guess_sequence_number(65536, 1), 65536);
        assert_eq!(guess_sequence_number(65534, 1), 65536);
        assert_eq!(guess_sequence_number(u16::MAX as u64 * 2 + 1, 1), 131071);
        assert_eq!(guess_sequence_number(65535, 65534), 65534);
        assert_eq!(guess_sequence_number(65534, 65534), 65534);
        assert_eq!(
            guess_sequence_number(u16::MAX as u64 * 2 + 1, 65534),
            131069
        );
    }
}
