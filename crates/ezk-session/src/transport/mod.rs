use std::{future::Future, io};

// mod direct_dtls_srtp;
mod direct_rtp;
mod task;

// pub(crate) use direct_dtls_srtp::DirectDtlsSrtpTransport;
pub(crate) use direct_rtp::DirectRtpTransport;
pub(crate) use task::{IdentifyableBy, TransportTaskHandle};

const RECV_BUFFER_SIZE: usize = 65535;

pub enum WhichTransport {
    Rtp,
    Rtcp,
}

pub trait RtpTransport: Send + 'static {
    type Event: Send;

    /// Receive data on the internal sockets
    ///
    /// Must be safe to cancel
    ///
    /// Returns the source address and on which transport the data was received on
    fn poll_event(&mut self) -> impl Future<Output = io::Result<Self::Event>> + Send;

    /// Handle received data, this function will not be cancelled by the task
    ///
    /// Returns whether or not the received data can be ignored by the task
    fn handle_event(
        &mut self,
        event: Self::Event,
    ) -> impl Future<Output = io::Result<Option<&[u8]>>> + Send;

    fn send_rtp(&mut self, buf: &mut Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
    fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> impl Future<Output = io::Result<()>> + Send {
        self.send_rtp(buf)
    }

    fn is_ready(&self) -> bool;
}

#[derive(Debug)]
pub enum PacketKind {
    Rtp,
    Rtcp,
    Stun,
    Dtls,
    Unknown,
}

impl PacketKind {
    fn identify(bytes: &[u8]) -> Self {
        if bytes.len() < 2 {
            return PacketKind::Unknown;
        }

        let byte = bytes[0];

        match byte {
            0 | 1 => PacketKind::Stun,
            20..=63 => PacketKind::Dtls,
            128..=191 => {
                let pt = bytes[1];

                if let 64..=95 = pt & 0x7F {
                    PacketKind::Rtcp
                } else {
                    PacketKind::Rtp
                }
            }
            _ => PacketKind::Unknown,
        }
    }
}
