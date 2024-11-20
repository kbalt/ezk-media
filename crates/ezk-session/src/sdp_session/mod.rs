use crate::{rtp_session::RtpSession, Codec, Codecs, TransceiverBuilder};
use sdp_types::{
    Connection, Direction, IceOptions, Media, MediaDescription, Origin, SessionDescription, Time,
};
use std::{collections::HashMap, net::IpAddr};

pub struct Session {
    sdp_id: u64,
    sdp_version: u64,

    address: IpAddr,

    next_media_id: LocalMediaId,
    local_media: HashMap<LocalMediaId, LocalMedia>,

    active: HashMap<LocalMediaId, Vec<ActiveMedia>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalMediaId(u32);

struct LocalMedia {
    codecs: Codecs,
    limit: usize,
}

struct ActiveMedia {
    rtp: RtpSession,

    codec: Codec,
}

impl Session {
    pub fn new(address: IpAddr) -> Self {
        Self {
            sdp_id: rand::random(),
            sdp_version: rand::random(),
            next_media_id: LocalMediaId(0),
            address,
            local_media: HashMap::new(),
            active: HashMap::new(),
        }
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(&mut self, codecs: Codecs, limit: usize) -> LocalMediaId {
        let id = self.next_media_id;
        self.next_media_id = LocalMediaId(id.0 + 1);

        self.local_media.insert(id, LocalMedia { codecs, limit });

        id
    }

    pub fn receiver_offer(&mut self, offer: SessionDescription) {
        for remote_media_description in offer.media_descriptions {
            for (local_media_id, local_media) in &mut self.local_media {
                if local_media.codecs.media_type != remote_media_description.media.media_type
                    || matches!(remote_media_description.direction, Direction::Inactive)
                {
                    continue;
                }

                // Build the transceiver
                let mut builder = TransceiverBuilder {
                    media_id: *local_media_id,
                    create_receive: None,
                    create_sender: None,
                };

                for codec in &mut local_media.codecs.codecs {
                    let use_codec = if let Some(static_pt) = codec.static_pt {
                        remote_media_description.media.fmts.contains(&static_pt)
                    } else {
                        remote_media_description.rtpmaps.iter().any(|rtpmap| {
                            rtpmap.encoding == codec.encoding_name.as_str()
                                && rtpmap.clock_rate == codec.clock_rate
                        })
                    };

                    if use_codec {
                        (codec.build)(&mut builder);
                        break;
                    }
                }

                if builder.create_sender.is_none() && builder.create_receive.is_none() {
                    // No codec found, skip
                    continue;
                }

                let (send, receive) = match remote_media_description.direction {
                    Direction::SendRecv => (true, true),
                    Direction::RecvOnly => (false, true),
                    Direction::SendOnly => (true, false),
                    Direction::Inactive => (false, false),
                };

                let mut rtp: Option<RtpSession> = None;

                if let Some(create_sender) = builder.create_sender.as_mut().filter(|_| send) {
                    let rtp = rtp.get_or_insert_with(RtpSession::new);
                    let sender = (create_sender)();
                    rtp.add_sender(sender)
                };

                if let Some(create_receiver) = builder.create_receive.as_mut().filter(|_| receive) {
                    let rtp = rtp.get_or_insert_with(RtpSession::new);
                    let receiver = rtp.add_receiver();
                    (create_receiver)(receiver);
                }

                if let Some(rtp) = rtp {
                    self.active
                        .entry(*local_media_id)
                        .or_default()
                        .push(ActiveMedia { rtp });
                }
            }
        }
    }

    pub fn create_sdp_answer(&self) -> SessionDescription {
        let media = self
            .active
            .iter()
            .flat_map(|(id, active)| active.iter().map(move |active| (id, active)))
            .map(|(id, active)| {
                let media_type = self.local_media[id].codecs.media_type;

                MediaDescription {
                    media: Media {
                        media_type,
                        port: todo!(),
                        ports_num: todo!(),
                        proto: todo!(),
                        fmts: todo!(),
                    },
                    direction: todo!(),
                    connection: todo!(),
                    bandwidth: todo!(),
                    rtcp_attr: todo!(),
                    rtpmaps: todo!(),
                    fmtps: todo!(),
                    ice_ufrag: todo!(),
                    ice_pwd: todo!(),
                    ice_candidates: todo!(),
                    ice_end_of_candidates: todo!(),
                    crypto: todo!(),
                    attributes: todo!(),
                }
            });

        SessionDescription {
            name: "-".into(),
            origin: Origin {
                username: "-".into(),
                session_id: self.sdp_id.to_string().into(),
                session_version: self.sdp_version.to_string().into(),
                address: self.address.into(),
            },
            time: Time { start: 0, stop: 0 },
            direction: Direction::SendRecv,
            connection: Some(Connection {
                address: self.address.into(),
                ttl: None,
                num: None,
            }),
            bandwidth: vec![],
            ice_options: IceOptions::default(),
            ice_lite: false,
            ice_ufrag: None,
            ice_pwd: None,
            attributes: vec![],
            media_descriptions: media.collect(),
        }
    }
}
