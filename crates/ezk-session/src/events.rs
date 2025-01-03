use crate::{MediaId, SocketId};
use ezk_rtp::RtpPacket;
use std::{collections::VecDeque, net::SocketAddr};

pub enum Event {
    /// Send data
    SendData {
        socket: SocketId,
        data: Vec<u8>,
        target: SocketAddr,
    },

    /// Connection state of the media has changed
    ///
    /// This is emitted for every track even if they are bundled on the same transport
    ConnectionState {
        media_id: MediaId,
        old: ConnectionState,
        new: ConnectionState,
    },

    /// Receive RTP on a track
    ReceiveRTP {
        media_id: MediaId,
        packet: RtpPacket,
    },
}
/// Connection state of a media track
///
/// Each track has its own connection state, since tracks may not always be bundled on the same transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Default)]
pub struct Events {
    events: VecDeque<Event>,
}

impl Events {
    pub fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn push(&mut self, event: Event) {
        self.events.push_back(event);
    }
}
