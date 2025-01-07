use crate::{MediaId, SocketId, TransportId};
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

/// Transport changes that have to be made before continuing with SDP negotiation.
/// These have to be handled before creating an SDP offer or answer.
pub enum TransportChange {
    /// The transport requests it's own UDP socket to be used
    ///
    /// The port of the socket must be reported using [`SdpSession::set_transport_ports`](super::SdpSession::set_transport_ports)
    CreateSocket(TransportId),
    /// Request for two UDP sockets to be created. One for RTP and RTCP each.
    /// Ideally the RTP port is an even port and the RTCP port is RTP port + 1
    ///
    /// The ports of the sockets must reported using [`SdpSession::set_transport_ports`](super::SdpSession::set_transport_ports)
    CreateSocketPair(TransportId),
    /// Remove the resources associated with the transport. Any pending data should still be sent.
    Remove(TransportId),
    /// Remove the RTCP socket of the given transport.
    RemoveRtcpSocket(TransportId),
}

// TODO; can this be removed because it too complex for something so simple
pub(crate) struct TransportRequiredChanges<'a> {
    pub(crate) id: TransportId,
    pub(crate) changes: &'a mut Vec<TransportChange>,
}

impl<'a> TransportRequiredChanges<'a> {
    pub(crate) fn new(id: TransportId, changes: &'a mut Vec<TransportChange>) -> Self {
        Self { id, changes }
    }

    pub(crate) fn require_socket(&mut self) {
        self.changes.push(TransportChange::CreateSocket(self.id))
    }

    pub(crate) fn require_socket_pair(&mut self) {
        self.changes
            .push(TransportChange::CreateSocketPair(self.id))
    }

    pub(crate) fn remove_rtcp_socket(&mut self) {
        self.changes
            .push(TransportChange::RemoveRtcpSocket(self.id));
    }
}
