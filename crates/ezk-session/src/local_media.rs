use crate::{Codec, Codecs, LocalMediaId, TransceiverBuilder};
use sdp_types::{Direction, MediaDescription};

pub(super) struct LocalMedia {
    pub(super) codecs: Codecs,
    pub(super) limit: usize,
    pub(super) use_count: usize,
}

impl LocalMedia {
    pub(super) fn maybe_use_for_offer(
        &mut self,
        self_id: LocalMediaId,
        m_line_index: usize,
        desc: &MediaDescription,
    ) -> Option<(Codec, u8)> {
        if self.limit == self.use_count || self.codecs.media_type != desc.media.media_type {
            return None;
        }

        // Try choosing a codec

        for entry in &mut self.codecs.codecs {
            let codec_pt = if let Some(static_pt) = entry.codec.static_pt {
                if desc.media.fmts.contains(&static_pt) {
                    Some(static_pt)
                } else {
                    None
                }
            } else {
                desc.rtpmap
                    .iter()
                    .find(|rtpmap| {
                        rtpmap.encoding == entry.codec.name.as_ref()
                            && rtpmap.clock_rate == entry.codec.clock_rate
                    })
                    .map(|rtpmap| rtpmap.payload)
            };

            if let Some(codec_pt) = codec_pt {
                let mut builder = TransceiverBuilder {
                    local_media_id: self_id,
                    m_line_index,
                    mid: desc.mid.clone(),
                    msid: None, // TODO: read msid

                    create_receiver: None,
                    create_sender: None,
                };

                (entry.build)(&mut builder);

                let has_sender = builder.create_sender.is_some();
                let has_receiver = builder.create_receiver.is_some();

                let (do_send, do_receive) = match desc.direction.flipped() {
                    Direction::SendRecv => (has_sender, has_receiver),
                    Direction::RecvOnly => (false, has_receiver),
                    Direction::SendOnly => (has_sender, false),
                    Direction::Inactive => (false, false),
                };

                if !(do_send || do_receive) {
                    // There would be no sender or receiver
                    return None;
                }

                self.use_count += 1; // TODO: decrement this
                return Some((entry.codec.clone(), codec_pt));
            }
        }

        None
    }
}
