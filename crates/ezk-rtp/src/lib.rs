use bytes::Bytes;
use ezk::{Frame, MediaType};

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
pub use session::RtpSession;

pub use rtcp_types;
pub use rtp_types;

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
