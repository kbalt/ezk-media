use bytesstr::BytesStr;
use ezk_rtp::RtpExtensionIds;
use sdp_types::{Direction, ExtMap, MediaDescription};

const RTP_MID_HDREXT: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

pub(crate) trait RtpExtensionIdsExt {
    fn offer() -> Self;
    fn from_sdp_media_description(desc: &MediaDescription) -> Self;
    fn to_extmap(&self) -> Vec<ExtMap>;
}

impl RtpExtensionIdsExt for RtpExtensionIds {
    fn offer() -> Self {
        RtpExtensionIds { mid: Some(1) }
    }

    fn from_sdp_media_description(desc: &MediaDescription) -> Self {
        RtpExtensionIds {
            mid: desc
                .extmap
                .iter()
                .find(|extmap| extmap.uri == RTP_MID_HDREXT)
                .map(|extmap| extmap.id),
        }
    }

    fn to_extmap(&self) -> Vec<ExtMap> {
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
