use bytesstr::BytesStr;
use ezk::Source;
use sdp_types::{
    Connection, Direction, ExtMap, Fmtp, Group, IceOptions, Media, MediaDescription, Origin, Rtcp,
    RtpMap, SessionDescription, TaggedAddress, Time, TransportProtocol,
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
};
use tokio::{net::lookup_host, try_join};
use transport::{DirectRtpTransport, IdentifyableBy, TransportTaskHandle};

mod codecs;
mod transceiver_builder;
mod transport;

pub use codecs::{Codec, Codecs};
pub use transceiver_builder::TransceiverBuilder;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct SdpSession {
    sdp_id: u64,
    sdp_version: u64,

    address: IpAddr,

    // Local configured media
    next_media_id: LocalMediaId,
    local_media: HashMap<LocalMediaId, LocalMedia>,

    // Active media
    next_active_media_id: ActiveMediaId,
    active_media: HashMap<LocalMediaId, Vec<ActiveMedia>>,

    // Transports
    next_transport_id: TransportId,
    transports: HashMap<TransportId, Transport>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalMediaId(u32);

impl LocalMediaId {
    fn step(&mut self) -> LocalMediaId {
        let id = *self;
        self.0 += 1;
        id
    }
}

struct LocalMedia {
    codecs: Codecs,
    limit: usize,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActiveMediaId(u32);

impl ActiveMediaId {
    fn step(&mut self) -> ActiveMediaId {
        let id = *self;
        self.0 += 1;
        id
    }
}

struct ActiveMedia {
    id: ActiveMediaId,

    mid: Option<BytesStr>,

    // position in the remote's sdp
    remote_pos: usize,

    transport: TransportId,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,

    codec_pt: u8,
    codec: Codec,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransportId(u32);

impl TransportId {
    fn step(&mut self) -> TransportId {
        let id = *self;
        self.0 += 1;
        id
    }
}

struct Transport {
    local_rtp_port: u16,
    local_rtcp_port: Option<u16>,
    handle: TransportTaskHandle,
}

impl SdpSession {
    pub fn new(address: IpAddr) -> Self {
        SdpSession {
            sdp_id: u64::from(rand::random::<u16>()),
            sdp_version: u64::from(rand::random::<u16>()),
            address,
            next_media_id: LocalMediaId(0),
            local_media: HashMap::new(),
            next_active_media_id: ActiveMediaId(0),
            active_media: HashMap::new(),
            next_transport_id: TransportId(0),
            transports: HashMap::new(),
        }
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(&mut self, codecs: Codecs, limit: usize) -> LocalMediaId {
        let id = self.next_media_id.step();

        self.local_media.insert(id, LocalMedia { codecs, limit });

        id
    }

    pub async fn receiver_offer(&mut self, offer: SessionDescription) -> Result<(), Error> {
        // TODO: rejected media must be included in the response with port=0
        for (remote_pos, remote_media_description) in offer.media_descriptions.iter().enumerate() {
            for (local_media_id, local_media) in &mut self.local_media {
                let active = self.active_media.entry(*local_media_id).or_default();

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
                    .find(|active| active.remote_rtp_address == remote_rtp_address);

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

                // Get or create transport for the m-line
                let transport_id = Self::get_or_create_transport(
                    &self.active_media,
                    &mut self.next_transport_id,
                    &mut self.transports,
                    &offer,
                    remote_media_description,
                    remote_rtp_address,
                    remote_rtcp_address,
                )
                .await?;

                let transport = self
                    .transports
                    .get_mut(&transport_id)
                    .expect("transport_id must be valid");

                let active_media_id = self.next_active_media_id.step();

                transport
                    .handle
                    .add_media_session(
                        active_media_id,
                        IdentifyableBy {
                            mid: None,
                            ssrc: vec![],
                            pt: vec![codec_pt],
                        },
                        codec.clock_rate,
                    )
                    .await;

                if let Some(create_sender) = builder.create_sender.as_mut().filter(|_| do_send) {
                    let sender = (create_sender)();
                    // TODO: do not call boxed here, the cancel safe requirement is too strict in transceiver builder
                    transport
                        .handle
                        .set_sender(active_media_id, sender.boxed(), codec_pt)
                        .await;
                };

                if let Some(create_receiver) =
                    builder.create_receiver.as_mut().filter(|_| do_receive)
                {
                    let receiver = transport
                        .handle
                        .set_receiver(active_media_id, codec_pt)
                        .await;

                    (create_receiver)(receiver);
                }

                self.active_media
                    .entry(*local_media_id)
                    .or_default()
                    .push(ActiveMedia {
                        id: active_media_id,
                        mid: remote_media_description.mid.clone(),
                        remote_pos,
                        transport: transport_id,
                        remote_rtp_address,
                        remote_rtcp_address: Some(remote_rtcp_address),
                        codec_pt,
                        codec,
                    });
            }
        }

        Ok(())
    }

    async fn get_or_create_transport(
        active_media: &HashMap<LocalMediaId, Vec<ActiveMedia>>,
        next_transport_id: &mut TransportId,
        transports: &mut HashMap<TransportId, Transport>,

        offer: &SessionDescription,
        remote_media_description: &MediaDescription,

        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<TransportId, Error> {
        match remote_media_description
            .mid
            .as_ref()
            .and_then(|mid| Self::find_bundled_transport(active_media, offer, mid))
        {
            Some(id) => {
                // Bundled transport found, use that one
                Ok(id)
            }

            None => {
                // Not bundled or no transport for the group created yet
                let mid_rtp_id = remote_media_description
                    .extmap
                    .iter()
                    .find(|extmap| extmap.uri == "urn:ietf:params:rtp-hdrext:sdes:mid")
                    .map(|extmap| extmap.id);

                let transport = DirectRtpTransport::new(
                    remote_rtp_address,
                    Some(remote_rtcp_address).filter(|_| !remote_media_description.rtcp_mux),
                )
                .await?;

                let local_rtp_port = transport.local_rtp_port();
                let local_rtcp_port = transport.local_rtcp_port();

                let handle = TransportTaskHandle::new(transport, mid_rtp_id).await?;

                let id = next_transport_id.step();

                transports.insert(
                    id,
                    Transport {
                        local_rtp_port,
                        local_rtcp_port,
                        handle,
                    },
                );

                Ok(id)
            }
        }
    }

    fn find_bundled_transport(
        active_media: &HashMap<LocalMediaId, Vec<ActiveMedia>>,
        offer: &SessionDescription,
        mid: &BytesStr,
    ) -> Option<TransportId> {
        let group = offer
            .group
            .iter()
            .find(|g| g.typ == "BUNDLE" && g.mids.contains(mid))?;

        active_media
            .values()
            .flat_map(|vec| vec.iter())
            .find_map(|m| {
                let mid = m.mid.as_ref()?;

                group
                    .mids
                    .iter()
                    .any(|gmid| gmid == mid.as_str())
                    .then_some(m.transport)
            })
    }

    pub fn create_sdp_answer(&self) -> SessionDescription {
        let mut media: Vec<(usize, MediaDescription)> = self
            .active_media
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

                let fmtps = active.codec.params.iter().map(|param| Fmtp {
                    format: active.codec_pt,
                    params: param.clone().into(),
                });

                let mut extmap = vec![];
                if active.mid.is_some() {
                    extmap.push(ExtMap {
                        id: 1, // TODO: have this not be hardcoded
                        uri: "urn:ietf:params:rtp-hdrext:sdes:mid".into(),
                        direction: Direction::SendRecv,
                    });
                }

                let transport = &self.transports[&active.transport];

                let media_description = MediaDescription {
                    media: Media {
                        media_type,
                        port: transport.local_rtp_port,
                        ports_num: None,
                        proto: TransportProtocol::RtpAvp,
                        fmts: vec![active.codec_pt],
                    },
                    direction: Direction::SendRecv, // TODO: set this correctly
                    connection: None,
                    bandwidth: vec![],
                    rtcp_attr: transport.local_rtcp_port.map(|port| Rtcp {
                        port,
                        address: None,
                    }),
                    rtcp_mux: true, // TODO: set accordingly
                    mid: active.mid.clone(),
                    rtpmaps: vec![rtpmap],
                    fmtps: fmtps.collect(),
                    ice_ufrag: None,
                    ice_pwd: None,
                    ice_candidates: vec![],
                    ice_end_of_candidates: false,
                    crypto: vec![],
                    extmap,
                    attributes: vec![],
                };

                (active.remote_pos, media_description)
            })
            .collect();

        media.sort_by_key(|(position, _)| *position);

        // Create bundle group attributes
        let group = {
            let mut bundle_groups: HashMap<TransportId, Vec<BytesStr>> =
                self.transports.keys().map(|id| (*id, vec![])).collect();

            for active_media in self.active_media.values().flatten() {
                bundle_groups
                    .get_mut(&active_media.transport)
                    .unwrap()
                    .push(active_media.mid.clone().unwrap());
            }

            bundle_groups.into_values().map(|mids| Group {
                typ: BytesStr::from_static("BUNDLE"),
                mids,
            })
        };

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
            group: group.collect(),
            extmap: vec![],
            ice_options: IceOptions::default(),
            ice_lite: false,
            ice_ufrag: None,
            ice_pwd: None,
            attributes: vec![],
            media_descriptions: media.into_iter().map(|(_, media)| media).collect(),
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

    let (remote_rtp_address, remote_rtcp_address) = try_join!(
        resolve_tagged_address(&remote_rtp_address, remote_rtp_port),
        resolve_tagged_address(&remote_rtcp_address, remote_rtcp_port),
    )?;

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
