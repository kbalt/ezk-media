use crate::{rtp_session::RtpSession, Codec, Codecs, TransceiverBuilder};
use sdp_types::{
    Connection, Direction, IceOptions, Media, MediaDescription, Origin, RtpMap, SessionDescription,
    TaggedAddress, Time,
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
};
use tokio::{join, net::lookup_host};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
}

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
    rtp_session: RtpSession,

    codec_pt: u8,
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

    pub async fn receiver_offer(&mut self, offer: SessionDescription) -> Result<(), Error> {
        for remote_media_description in &offer.media_descriptions {
            for (local_media_id, local_media) in &mut self.local_media {
                let active = self.active.entry(*local_media_id).or_default();

                if local_media.codecs.media_type != remote_media_description.media.media_type
                    || matches!(remote_media_description.direction, Direction::Inactive)
                {
                    continue;
                }

                // resolve remote rtp & rtcp address
                let (remote_rtp_address, remote_rtcp_address) =
                    resolve_rtp_and_rtcp_address(&offer, remote_media_description).await?;

                // Find out if this offer is part of an active session
                let active_media = active
                    .iter()
                    .find(|active| active.rtp_session.remote_rtcp_address() == remote_rtcp_address);

                if active_media.is_some() {
                    // TODO: verify that the media is still valid (codec might need to change)
                    continue;
                }

                if active.len() >= local_media.limit {
                    // Cannot create more media sessions using this local media
                    continue;
                }

                let Some((mut builder, codec, codec_pt)) =
                    choose_codec(remote_media_description, local_media, local_media_id)
                else {
                    // No codec found, skip
                    continue;
                };

                let (do_send, do_receive) = match remote_media_description.direction.flipped() {
                    Direction::SendRecv => (true, true),
                    Direction::RecvOnly => (false, true),
                    Direction::SendOnly => (true, false),
                    Direction::Inactive => (false, false),
                };

                if !(do_send && builder.create_sender.is_some()
                    || do_receive && builder.create_receiver.is_some())
                {
                    // There would be no sender or receiver, skip
                    continue;
                }

                // Create rtp session
                let rtcp_mux = remote_media_description
                    .attributes
                    .iter()
                    .any(|attr| attr.name == "rtcp-mux");

                let mut rtp_session = RtpSession::new(remote_rtcp_address, rtcp_mux).await?;

                if let Some(create_sender) = builder.create_sender.as_mut().filter(|_| do_send) {
                    let sender = (create_sender)();
                    rtp_session.set_sender(sender, remote_rtp_address);
                };

                if let Some(create_receiver) =
                    builder.create_receiver.as_mut().filter(|_| do_receive)
                {
                    let receiver = rtp_session.set_receiver();

                    (create_receiver)(receiver);
                }

                self.active
                    .entry(*local_media_id)
                    .or_default()
                    .push(ActiveMedia {
                        rtp_session,
                        codec_pt,
                        codec,
                    });
            }
        }

        Ok(())
    }

    pub fn create_sdp_answer(&self) -> SessionDescription {
        let media = self
            .active
            .iter()
            .flat_map(|(id, active)| active.iter().map(move |active| (id, active)))
            .map(|(id, active)| {
                let media_type = self.local_media[id].codecs.media_type;

                let rtpmap = RtpMap {
                    payload: active.codec_pt,
                    encoding: active.codec.name.as_str().into(),
                    clock_rate: active.codec.clock_rate,
                    params: Default::default(),
                };

                MediaDescription {
                    media: Media {
                        media_type,
                        port: todo!(),
                        ports_num: None,
                        proto: todo!(),
                        fmts: todo!(),
                    },
                    direction: todo!(),
                    connection: todo!(),
                    bandwidth: todo!(),
                    rtcp_attr: todo!(),
                    rtpmaps: vec![rtpmap],
                    fmtps: todo!(),
                    ice_ufrag: todo!(),
                    ice_pwd: todo!(),
                    ice_candidates: todo!(),
                    ice_end_of_candidates: todo!(),
                    crypto: todo!(),
                    attributes: vec![],
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

fn choose_codec(
    remote_media_description: &MediaDescription,
    local_media: &mut LocalMedia,
    local_media_id: &LocalMediaId,
) -> Option<(TransceiverBuilder, Codec, u8)> {
    let mut chosen_codec = None;

    for entry in &mut local_media.codecs.codecs {
        let codec_pt = if let Some(static_pt) = entry.codec.static_pt {
            if remote_media_description.media.fmts.contains(&static_pt) {
                Some(static_pt)
            } else {
                None
            }
        } else {
            remote_media_description
                .rtpmaps
                .iter()
                .find(|rtpmap| {
                    rtpmap.encoding == entry.codec.name.as_str()
                        && rtpmap.clock_rate == entry.codec.clock_rate
                })
                .map(|rtpmap| rtpmap.payload)
        };

        if let Some(codec_pt) = codec_pt {
            let mut builder = TransceiverBuilder {
                local_media_id: *local_media_id,
                create_receiver: None,
                create_sender: None,
            };

            (entry.build)(&mut builder);

            chosen_codec = Some((builder, entry.codec.clone(), codec_pt));

            break;
        }
    }

    chosen_codec
}

async fn resolve_rtp_and_rtcp_address(
    offer: &SessionDescription,
    remote_media_description: &MediaDescription,
) -> Result<(SocketAddr, SocketAddr), Error> {
    let connection = offer
        .connection
        .as_ref()
        .or(remote_media_description.connection.as_ref())
        .unwrap();

    let remote_rtp_address = connection.address.clone();
    let remote_rtp_port = remote_media_description.media.port;

    let (remote_rtcp_address, remote_rtcp_port) =
        rtcp_address_and_port(remote_media_description, connection);

    let (remote_rtp_address, remote_rtcp_address) = join!(
        resolve_tagged_address(&remote_rtp_address, remote_rtp_port),
        resolve_tagged_address(&remote_rtcp_address, remote_rtcp_port),
    );

    let remote_rtp_address = remote_rtp_address?;
    let remote_rtcp_address = remote_rtcp_address?;

    Ok((remote_rtp_address, remote_rtcp_address))
}

fn rtcp_address_and_port(
    remote_media_description: &MediaDescription,
    connection: &Connection,
) -> (TaggedAddress, u16) {
    if let Some(rtcp_addr) = &remote_media_description.rtcp_attr {
        let address = rtcp_addr
            .address
            .clone()
            .unwrap_or_else(|| connection.address.clone());

        (address, rtcp_addr.port)
    } else {
        let rtcp_mux = remote_media_description
            .attributes
            .iter()
            .any(|attr| attr.name == "rtcp-mux");

        let rtcp_port = if !rtcp_mux {
            remote_media_description.media.port
        } else {
            remote_media_description.media.port + 1
        };

        (connection.address.clone(), rtcp_port)
    }
}

async fn resolve_tagged_address(address: &TaggedAddress, port: u16) -> io::Result<SocketAddr> {
    match address {
        TaggedAddress::IP4(ipv4_addr) => Ok(SocketAddr::from((*ipv4_addr, port))),
        TaggedAddress::IP4FQDN(bytes_str) => lookup_host((bytes_str.as_str(), port))
            .await?
            .find(|ip| ip.is_ipv4())
            .ok_or(io::Error::other(format!(
                "Failed to find IPv4 address for {bytes_str}"
            ))),
        TaggedAddress::IP6(ipv6_addr) => Ok(SocketAddr::from((*ipv6_addr, port))),
        TaggedAddress::IP6FQDN(bytes_str) => lookup_host((bytes_str.as_str(), port))
            .await?
            .find(|ip| ip.is_ipv6())
            .ok_or(io::Error::other(format!(
                "Failed to find IPv6 address for {bytes_str}"
            ))),
    }
}
