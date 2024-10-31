use bytes::Bytes;
use ezk::{Frame, MediaType};
use std::str::Utf8Error;

mod depacketizer;
mod media_type;
mod ntp_timestamp;
mod packetizer;
mod rtp_packet;
mod session;

pub use depacketizer::DePacketizer;
pub use media_type::{Rtp, RtpConfig, RtpConfigRange};
pub use ntp_timestamp::NtpTimestamp;
pub use packetizer::Packetizer;
pub use rtp_packet::*;
pub use session::Session;

pub use rtcp_types;
pub use rtp_types;

#[derive(Debug)]
pub enum DecodeError {
    Incomplete,
    InvalidVersion,
    InvalidAlignment,

    UnknownPayloadType(u8),
    UnknownFmt(u8),
    UnknownTag(u8),

    Utf8(Utf8Error),
}

impl From<Utf8Error> for DecodeError {
    fn from(value: Utf8Error) -> Self {
        Self::Utf8(value)
    }
}

/// Create RTP payload from media data
pub trait Payloader<M: MediaType>: Send + 'static {
    /// Payload a given frame
    fn payload(&mut self, frame: Frame<M>) -> impl Iterator<Item = Bytes> + '_;
}

/// A media type that can be packed into RTP packets
///
/// Usually encoded audio or video
pub trait Payloadable: Sized + MediaType {
    /// Payloader implementation to use
    type Payloader: Payloader<Self>;
    type DePayloader: DePayloader<Self>;

    /// Statically assigned payload type
    const STATIC_PT: Option<u8>;

    /// Create the payload with the given configuration
    fn make_payloader(config: Self::Config) -> Self::Payloader;

    /// Create a depayloader and negotiate
    fn make_depayloader(available: Vec<Self::ConfigRange>) -> (Self::Config, Self::DePayloader);
}

pub trait DePayloader<M: MediaType>: Send + 'static {
    // TODO: temporary API until I know how this should work
    fn depayload(&mut self, payload: &[u8]) -> M::FrameData;
}
