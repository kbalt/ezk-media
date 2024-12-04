use core::fmt;

/// Owned wrapper around [`rtp_types::RtpPacket`]
#[derive(Clone)]
pub struct RtpPacket(Vec<u8>);

impl fmt::Debug for RtpPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

impl RtpPacket {
    pub fn new(packet: &rtp_types::RtpPacketBuilder<&[u8], &[u8]>) -> Self {
        Self(packet.write_vec_unchecked())
    }

    pub fn parse(i: &[u8]) -> Result<Self, rtp_types::RtpParseError> {
        let _packet = rtp_types::RtpPacket::parse(i)?;

        Ok(Self(i.to_vec()))
    }

    pub fn get(&self) -> rtp_types::RtpPacket<'_> {
        rtp_types::RtpPacket::parse(&self.0)
            .expect("internal buffer must contain a valid rtp packet")
    }

    pub fn get_mut(&mut self) -> rtp_types::RtpPacketMut<'_> {
        rtp_types::RtpPacketMut::parse(&mut self.0[..])
            .expect("internal buffer must contain a valid rtp packet")
    }
}
