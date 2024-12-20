use std::{future::Future, io};
use stun_types::{is_stun_message, IsStunMessageInfo};

mod direct_rtp;
mod direct_srtp;
mod dtls_srtp;
mod task;

pub(crate) use direct_rtp::DirectRtpTransport;
pub(crate) use direct_srtp::{DirectSrtpTransport, DtlsSetup};
pub(crate) use task::{IdentifyableBy, TransportTaskHandle};

const RECV_BUFFER_SIZE: usize = 65535;

pub trait RtpTransport: Send + 'static {
    fn recv(&mut self, buf: &mut Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
    fn send_rtp(&mut self, buf: &mut Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
    fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> impl Future<Output = io::Result<()>> + Send {
        self.send_rtp(buf)
    }

    fn is_ready(&self) -> bool;
}

pub enum PacketKind {
    Rtp,
    Rtcp,
    Stun,
    Unknown,
}

impl PacketKind {
    fn identify(bytes: &[u8]) -> Self {
        if let IsStunMessageInfo::Yes { .. } = is_stun_message(bytes) {
            return PacketKind::Stun;
        }

        if bytes.len() < 2 {
            return PacketKind::Unknown;
        }

        let pt = bytes[1];

        if let 64..=95 = pt & 0x7F {
            PacketKind::Rtcp
        } else {
            PacketKind::Rtp
        }
    }
}
