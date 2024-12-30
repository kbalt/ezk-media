use crate::{ActiveMediaId, SocketId};
use ezk_rtp::RtpPacket;
use std::{collections::VecDeque, net::SocketAddr};

pub enum Event {
    /// Create 2 UdpSockets for a media session
    ///
    /// This is called when rtcp-mux is not available
    CreateUdpSocketPair { socket_ids: [SocketId; 2] },

    /// Create a single UdpSocket for a media session
    CreateUdpSocket { socket_id: SocketId },

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
        media_id: ActiveMediaId,
        state: ConnectionState,
    },

    /// Receive RTP on a track
    ReceiveRTP {
        media_id: ActiveMediaId,
        packet: RtpPacket,
    },
}
/// Connection state of a media track
///
/// Each track has a connection state since tracks may not always be bundled on the same transport.
#[derive(Debug)]
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
