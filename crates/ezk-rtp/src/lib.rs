use bytes::Bytes;
use ezk::{Frame, MediaType};

mod depacketizer;
mod extensions;
mod media_type;
mod ntp_timestamp;
mod packetizer;
mod rtp_packet;
mod session;

pub use depacketizer::DePacketizer;
pub use extensions::{parse_extensions, RtpExtensionsWriter};
pub use media_type::{Rtp, RtpConfig, RtpConfigRange};
pub use ntp_timestamp::NtpTimestamp;
pub use packetizer::Packetizer;
pub use rtp_packet::{RtpExtensionIds, RtpExtensions, RtpPacket};
pub use session::RtpSession;

pub use rtcp_types;
pub use rtp_types;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ssrc(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceNumber(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtendedSequenceNumber(pub u64);

impl ExtendedSequenceNumber {
    pub fn increase_one(&mut self) -> SequenceNumber {
        self.0 += 1;
        SequenceNumber((self.0 & u16::MAX as u64) as u16)
    }

    pub fn rollover_count(&self) -> u64 {
        self.0 >> 16
    }

    pub fn guess_extended(&self, seq: SequenceNumber) -> ExtendedSequenceNumber {
        ExtendedSequenceNumber(wrapping_counter_to_u64_counter(
            self.0,
            u64::from(seq.0),
            u64::from(u16::MAX),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RtpTimestamp(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtendedRtpTimestamp(pub u64);

impl ExtendedRtpTimestamp {
    pub fn truncated(&self) -> RtpTimestamp {
        RtpTimestamp(self.0 as u32)
    }

    pub fn rollover_count(&self) -> u64 {
        self.0 >> 32
    }

    pub fn guess_extended(&self, seq: RtpTimestamp) -> ExtendedRtpTimestamp {
        ExtendedRtpTimestamp(wrapping_counter_to_u64_counter(
            self.0,
            u64::from(seq.0),
            u64::from(u32::MAX),
        ))
    }
}

/// A media type that can be packed into RTP packets
///
/// Usually encoded audio or video
pub trait Payloadable: Sized + MediaType {
    type Payloader: Payloader<Self>;
    type DePayloader: DePayloader<Self>;

    /// Statically assigned payload type
    const STATIC_PT: Option<u8>;

    /// Create the payload with the given configuration
    fn make_payloader(config: Self::Config) -> Self::Payloader;

    /// Create a depayloader and negotiate
    fn make_depayloader(available: Vec<Self::ConfigRange>) -> (Self::Config, Self::DePayloader);
}

/// Create RTP payload from media data
pub trait Payloader<M: MediaType>: Send + 'static {
    /// Payload a given frame
    fn payload(&mut self, frame: Frame<M>, max_size: usize) -> impl Iterator<Item = Bytes> + '_;
}

pub trait DePayloader<M: MediaType>: Send + 'static {
    fn depayload(&mut self, payload: &[u8]) -> M::FrameData;
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
