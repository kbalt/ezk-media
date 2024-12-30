use bytesstr::BytesStr;
use ezk_rtp::{
    parse_extensions,
    rtp_types::{RtpPacket, RtpPacketBuilder},
    RtpExtensionsWriter,
};
use sdp_types::{Direction, ExtMap, MediaDescription};

const RTP_MID_HDREXT: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

pub(crate) struct RtpExtensions<'a> {
    pub(crate) mid: Option<&'a [u8]>,
}

impl<'a> RtpExtensions<'a> {
    pub(crate) fn from_packet(ids: &RtpExtensionIds, packet: &'a RtpPacket<'a>) -> Self {
        let mut this = Self { mid: None };

        let Some((profile, data)) = packet.extension() else {
            return this;
        };

        for (id, data) in parse_extensions(profile, data) {
            if Some(id) == ids.mid {
                this.mid = Some(data);
            }
        }

        this
    }

    /// Write the extension data out and returns the profile-id if anything has been written
    pub(crate) fn write<'b>(
        &self,
        ids: &RtpExtensionIds,
        packet_builder: RtpPacketBuilder<&'b [u8], Vec<u8>>,
    ) -> RtpPacketBuilder<&'b [u8], Vec<u8>> {
        let Some((id, mid)) = ids.mid.zip(self.mid) else {
            return packet_builder;
        };

        let mut buf = vec![];

        let profile = RtpExtensionsWriter::new(&mut buf, mid.len() <= 16)
            .with(id, mid)
            .finish();

        packet_builder.extension(profile, buf)
    }
}

pub(crate) struct RtpExtensionIds {
    pub(crate) mid: Option<u8>,
}

impl RtpExtensionIds {
    pub(crate) fn from_offer(offer: &MediaDescription) -> Self {
        Self {
            mid: offer
                .extmap
                .iter()
                .find(|extmap| extmap.uri == RTP_MID_HDREXT)
                .map(|extmap| extmap.id),
        }
    }

    pub(crate) fn to_extmap(&self) -> Vec<ExtMap> {
        let mut extmap = vec![];

        if let Some(mid_id) = self.mid {
            extmap.push(ExtMap {
                id: mid_id,
                uri: BytesStr::from_static(RTP_MID_HDREXT),
                direction: Direction::SendRecv,
            });
        }

        extmap
    }
}
