use crate::{Codec, Codecs, DirectionBools};
use sdp_types::{Direction, MediaDescription};

pub(super) struct LocalMedia {
    pub(super) codecs: Codecs,
    pub(super) limit: usize,
    pub(super) direction: DirectionBools,
    pub(super) use_count: usize,
}

impl LocalMedia {
    pub(super) fn maybe_use_for_offer(
        &mut self,
        desc: &MediaDescription,
    ) -> Option<(Codec, u8, DirectionBools)> {
        if self.limit == self.use_count || self.codecs.media_type != desc.media.media_type {
            return None;
        }

        // Try choosing a codec

        for codec in &mut self.codecs.codecs {
            let codec_pt = if let Some(static_pt) = codec.pt {
                if desc.media.fmts.contains(&static_pt) {
                    Some(static_pt)
                } else {
                    None
                }
            } else {
                desc.rtpmap
                    .iter()
                    .find(|rtpmap| {
                        rtpmap.encoding == codec.name.as_ref()
                            && rtpmap.clock_rate == codec.clock_rate
                    })
                    .map(|rtpmap| rtpmap.payload)
            };

            if let Some(codec_pt) = codec_pt {
                let (do_send, do_receive) = match desc.direction.flipped() {
                    Direction::SendRecv => (self.direction.send, self.direction.recv),
                    Direction::RecvOnly => (false, self.direction.recv),
                    Direction::SendOnly => (self.direction.send, false),
                    Direction::Inactive => (false, false),
                };

                if !(do_send || do_receive) {
                    // There would be no sender or receiver
                    return None;
                }

                self.use_count += 1;

                return Some((
                    codec.clone(),
                    codec_pt,
                    DirectionBools {
                        send: do_send,
                        recv: do_receive,
                    },
                ));
            }
        }

        None
    }
}
