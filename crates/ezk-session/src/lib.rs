use bytesstr::BytesStr;
use sdp_types::{
    Connection, Direction, ExtMap, Fmtp, Group, IceOptions, Media, MediaDescription, MediaType,
    Origin, Rtcp, RtpMap, SessionDescription, TaggedAddress, Time, TransportProtocol,
};
use std::{
    collections::HashMap,
    io,
    mem::replace,
    net::{IpAddr, SocketAddr},
};
use tokio::{net::lookup_host, sync::mpsc, try_join};
use transport::{DirectRtpTransport, IdentifyableBy, TransportTaskHandle};

mod codecs;
mod transceiver_builder;
mod transport;

pub use codecs::{Codec, Codecs};
pub use transceiver_builder::TransceiverBuilder;

// TODO: have this not be hardcoded
const RTP_MID_HDREXT_ID: u8 = 1;
const RTP_MID_HDREXT: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

macro_rules! id_types {
    ($($name:ident),* $(,)?) => {
        $(
        #[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            fn step(&mut self) -> Self {
                let id = *self;
                self.0 += 1;
                id
            }
        }
        )*
    };
}

id_types! {
    LocalMediaId,
    ActiveMediaId,
    TransportId,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct SdpSession {
    sdp_id: u64,
    sdp_version: u64,

    // Local ip address to use
    address: IpAddr,

    // Local configured media
    next_media_id: LocalMediaId,
    local_media: HashMap<LocalMediaId, LocalMedia>,

    // Active media
    next_active_media_id: ActiveMediaId, // TODO: this will be used to set the `mid` and `msid?` when creating offers?
    state: Vec<MediaEntry>,

    // Transports
    next_transport_id: TransportId,
    transports: HashMap<TransportId, Transport>,
}

struct LocalMedia {
    codecs: Codecs,
    limit: usize,
    use_count: usize,
}

impl LocalMedia {
    fn maybe_use(
        &mut self,
        self_id: LocalMediaId,
        m_line_index: usize,
        desc: &MediaDescription,
    ) -> Option<(TransceiverBuilder, Codec, u8)> {
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
                return Some((builder, entry.codec.clone(), codec_pt));
            }
        }

        None
    }
}

enum MediaEntry {
    Active(ActiveMedia),
    Rejected(MediaType),
}

impl MediaEntry {
    fn active(&self) -> Option<&ActiveMedia> {
        match self {
            MediaEntry::Active(active_media) => Some(active_media),
            MediaEntry::Rejected(..) => None,
        }
    }
}

struct ActiveMedia {
    id: ActiveMediaId,
    local_media_id: LocalMediaId,

    media_type: MediaType,

    /// Optional mid
    mid: Option<BytesStr>,
    direction: Direction2,

    transport: TransportId,
    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,

    codec_pt: u8,
    codec: Codec,
    builder: TransceiverBuilder,
}

impl ActiveMedia {
    fn matches(&self, desc: &MediaDescription) -> bool {
        if self.media_type != desc.media.media_type {
            return false;
        }

        if let Some((self_mid, desc_mid)) = self.mid.as_ref().zip(desc.mid.as_ref()) {
            return self_mid == desc_mid;
        }

        self.remote_rtp_address.port() == desc.media.port
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
            state: Vec::new(),
            next_transport_id: TransportId(0),
            transports: HashMap::new(),
        }
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(&mut self, codecs: Codecs, limit: usize) -> LocalMediaId {
        let id = self.next_media_id.step();
        self.local_media.insert(
            id,
            LocalMedia {
                codecs,
                limit,
                use_count: 0,
            },
        );
        id
    }

    pub async fn receiver_offer(&mut self, offer: SessionDescription) -> Result<(), Error> {
        let mut new_state = vec![];

        for (m_line_index, remote_media_description) in offer.media_descriptions.iter().enumerate()
        {
            let requested_direction: Direction2 =
                remote_media_description.direction.flipped().into();

            // First thing: Search the current state for an entry that matches this description - and update accordingly
            let matched_position = self.state.iter().position(|e| match e {
                MediaEntry::Active(active_media) => active_media.matches(remote_media_description),
                MediaEntry::Rejected(media_type) => {
                    *media_type == remote_media_description.media.media_type
                }
            });

            if let Some(position) = matched_position {
                // Remove the entry and only if its active, we don't consider creating a new media session
                if let MediaEntry::Active(mut active_media) = self.state.remove(position) {
                    self.update_active_media(requested_direction, &mut active_media)
                        .await;
                    new_state.push(MediaEntry::Active(active_media));
                    continue;
                }
            }

            // Reject any invalid/inactive m-lines
            if remote_media_description.direction == Direction::Inactive
                || remote_media_description.media.port == 0
            {
                new_state.push(MediaEntry::Rejected(
                    remote_media_description.media.media_type,
                ));

                continue;
            }

            // resolve remote rtp & rtcp address
            let (remote_rtp_address, remote_rtcp_address) =
                resolve_rtp_and_rtcp_address(&offer, remote_media_description).await?;

            // Choose local media for this m-line
            let Some((local_media_id, (mut builder, codec, codec_pt))) =
                self.local_media.iter_mut().find_map(|(id, local_media)| {
                    let config =
                        local_media.maybe_use(*id, m_line_index, remote_media_description)?;

                    Some((*id, config))
                })
            else {
                // no local media found for this
                new_state.push(MediaEntry::Rejected(
                    remote_media_description.media.media_type,
                ));

                continue;
            };

            let do_send = builder.create_sender.is_some() && requested_direction.send;
            let do_recv = builder.create_receiver.is_some() && requested_direction.recv;

            // Get or create transport for the m-line
            let transport_id = self
                .get_or_create_transport(
                    &new_state,
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
                        mid: remote_media_description.mid.clone(),
                        ssrc: vec![], // TODO: read ssrc attributes
                        pt: vec![codec_pt],
                    },
                    codec.clock_rate,
                )
                .await;

            if do_send {
                let create_sender = builder.create_sender.as_mut().unwrap();
                let (tx, rx) = mpsc::channel(8);
                transport.handle.set_sender(active_media_id, rx).await;
                (create_sender)(tx);
            }

            if do_recv {
                let create_receiver = builder.create_receiver.as_mut().unwrap();
                let (tx, rx) = mpsc::channel(8);
                transport.handle.set_receiver(active_media_id, tx).await;
                (create_receiver)(rx);
            }

            new_state.push(MediaEntry::Active(ActiveMedia {
                id: active_media_id,
                local_media_id,
                media_type: remote_media_description.media.media_type,
                mid: remote_media_description.mid.clone(),
                direction: Direction2 {
                    send: do_send,
                    recv: do_recv,
                },
                transport: transport_id,
                remote_rtp_address,
                remote_rtcp_address: Some(remote_rtcp_address),
                codec_pt,
                codec,
                builder,
            }));
        }

        let old_state = replace(&mut self.state, new_state);

        for entry in old_state {
            if let MediaEntry::Active(active) = entry {
                self.local_media
                    .get_mut(&active.local_media_id)
                    .unwrap()
                    .use_count -= 1;
            }
        }

        Ok(())
    }

    async fn update_active_media(
        &mut self,
        requested_direction: Direction2,
        active_media: &mut ActiveMedia,
    ) {
        let transport = self.transports.get_mut(&active_media.transport).unwrap();

        // If remote wants to receive data, but we're not sending anything
        if requested_direction.send && !active_media.direction.send {
            if let Some(create_sender) = &mut active_media.builder.create_sender {
                let (tx, rx) = mpsc::channel(8);
                transport.handle.set_sender(active_media.id, rx).await;
                create_sender(tx);
            }
        }

        // If remote is not receiving anything and we're sending, we need to stop sending
        if !requested_direction.send
            && active_media.direction.send
            && active_media.builder.create_sender.is_some()
        {
            transport.handle.remove_sender(active_media.id).await;
        }

        // If remote wants to send us something, but we're not receiving yet
        if requested_direction.recv && !active_media.direction.recv {
            if let Some(create_receiver) = &mut active_media.builder.create_receiver {
                let (tx, rx) = mpsc::channel(8);
                transport.handle.set_receiver(active_media.id, tx).await;
                create_receiver(rx);
            }
        }

        // If remote is not sending anything and we're received, we need to stop receiving
        if !requested_direction.recv
            && active_media.direction.recv
            && active_media.builder.create_receiver.is_some()
        {
            transport.handle.remove_receiver(active_media.id).await;
        }

        active_media.direction.send =
            requested_direction.send && active_media.builder.create_sender.is_some();
        active_media.direction.recv =
            requested_direction.recv && active_media.builder.create_receiver.is_some();
    }

    /// Get or create a transport for the given media description
    async fn get_or_create_transport(
        &mut self,
        new_state: &[MediaEntry],
        offer: &SessionDescription,
        remote_media_description: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<TransportId, Error> {
        match remote_media_description
            .mid
            .as_ref()
            .and_then(|mid| self.find_bundled_transport(new_state, offer, mid))
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
                    .find(|extmap| extmap.uri == RTP_MID_HDREXT)
                    .map(|extmap| extmap.id);

                let transport = DirectRtpTransport::new(
                    remote_rtp_address,
                    Some(remote_rtcp_address).filter(|_| !remote_media_description.rtcp_mux),
                )
                .await?;

                let local_rtp_port = transport.local_rtp_port();
                let local_rtcp_port = transport.local_rtcp_port();

                let handle = TransportTaskHandle::new(transport, mid_rtp_id).await?;

                let id = self.next_transport_id.step();

                self.transports.insert(
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
        &self,
        new_state: &[MediaEntry],
        offer: &SessionDescription,
        mid: &BytesStr,
    ) -> Option<TransportId> {
        let group = offer
            .group
            .iter()
            .find(|g| g.typ == "BUNDLE" && g.mids.contains(mid))?;

        new_state
            .iter()
            .chain(&self.state)
            .filter_map(MediaEntry::active)
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
        let media = self.state.iter().map(|entry| {
            let active = match entry {
                MediaEntry::Active(active_media) => active_media,
                MediaEntry::Rejected(media_type) => return rejected_media_description(media_type),
            };

            let rtpmap = RtpMap {
                payload: active.codec_pt,
                encoding: active.codec.name.as_ref().into(),
                clock_rate: active.codec.clock_rate,
                params: Default::default(),
            };

            let fmtps = active.codec.param.iter().map(|param| Fmtp {
                format: active.codec_pt,
                params: param.as_ref().into(),
            });

            let mut extmap = vec![];
            if active.mid.is_some() {
                extmap.push(ExtMap {
                    id: RTP_MID_HDREXT_ID,
                    uri: BytesStr::from_static(RTP_MID_HDREXT),
                    direction: Direction::SendRecv,
                });
            }

            let transport = &self.transports[&active.transport];

            MediaDescription {
                media: Media {
                    media_type: active.media_type,
                    port: transport.local_rtp_port,
                    ports_num: None,
                    proto: TransportProtocol::RtpAvp,
                    fmts: vec![active.codec_pt],
                },
                connection: None,
                bandwidth: vec![],
                direction: active.direction.into(),
                rtcp: transport.local_rtcp_port.map(|port| Rtcp {
                    port,
                    address: None,
                }),
                rtcp_mux: active.remote_rtcp_address.is_none(),
                mid: active.mid.clone(),
                rtpmap: vec![rtpmap],
                fmtp: fmtps.collect(),
                ice_ufrag: None,
                ice_pwd: None,
                ice_candidates: vec![],
                ice_end_of_candidates: false,
                crypto: vec![],
                extmap,
                attributes: vec![],
            }
        });

        // Create bundle group attributes
        let group = {
            let mut bundle_groups: HashMap<TransportId, Vec<BytesStr>> = HashMap::new();

            for active_media in self.state.iter().filter_map(MediaEntry::active) {
                if let Some(mid) = active_media.mid.clone() {
                    bundle_groups
                        .entry(active_media.transport)
                        .or_default()
                        .push(mid);
                }
            }

            bundle_groups
                .into_values()
                .filter(|c| c.len() > 1)
                .map(|mids| Group {
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
            media_descriptions: media.collect(),
        }
    }
}

async fn resolve_rtp_and_rtcp_address(
    offer: &SessionDescription,
    remote_media_description: &MediaDescription,
) -> Result<(SocketAddr, SocketAddr), Error> {
    let connection = remote_media_description
        .connection
        .as_ref()
        .or(offer.connection.as_ref())
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
    if remote_media_description.rtcp_mux {
        return (
            connection.address.clone(),
            remote_media_description.media.port,
        );
    }

    if let Some(rtcp_addr) = &remote_media_description.rtcp {
        let address = rtcp_addr
            .address
            .clone()
            .unwrap_or_else(|| connection.address.clone());

        return (address, rtcp_addr.port);
    }

    (
        connection.address.clone(),
        remote_media_description.media.port + 1,
    )
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

fn rejected_media_description(media_type: &MediaType) -> MediaDescription {
    MediaDescription {
        media: Media {
            media_type: *media_type,
            port: 0,
            ports_num: None,
            proto: TransportProtocol::RtpAvp,
            fmts: vec![],
        },
        connection: None,
        bandwidth: vec![],
        direction: Direction::Inactive,
        rtcp: None,
        rtcp_mux: false,
        mid: None,
        rtpmap: vec![],
        fmtp: vec![],
        ice_ufrag: None,
        ice_pwd: None,
        ice_candidates: vec![],
        ice_end_of_candidates: false,
        crypto: vec![],
        extmap: vec![],
        attributes: vec![],
    }
}

#[derive(Debug, Clone, Copy)]
struct Direction2 {
    send: bool,
    recv: bool,
}

impl From<Direction2> for Direction {
    fn from(value: Direction2) -> Self {
        match (value.send, value.recv) {
            (true, true) => Direction::SendRecv,
            (true, false) => Direction::SendOnly,
            (false, true) => Direction::RecvOnly,
            (false, false) => Direction::Inactive,
        }
    }
}

impl From<Direction> for Direction2 {
    fn from(value: Direction) -> Self {
        let (send, recv) = match value {
            Direction::SendRecv => (true, true),
            Direction::RecvOnly => (false, true),
            Direction::SendOnly => (true, false),
            Direction::Inactive => (false, false),
        };

        Self { send, recv }
    }
}
