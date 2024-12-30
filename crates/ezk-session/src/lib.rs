use bytesstr::BytesStr;
use ezk_rtp::{rtcp_types::Compound, RtpPacket, RtpSession};
use local_media::LocalMedia;
use rtp::RtpExtensions;
use sdp_types::{
    Connection, Direction, Fmtp, Group, IceOptions, Media, MediaDescription, MediaType, Origin,
    Rtcp, RtpMap, SessionDescription, TaggedAddress, Time, TransportProtocol,
};
use slotmap::SlotMap;
use std::{
    borrow::Cow,
    cmp::min,
    collections::{HashMap, VecDeque},
    io,
    mem::replace,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use transport::{ReceivedPacket, Transport};

mod codecs;
mod events;
mod local_media;
mod rtp;
mod transceiver_builder;
mod transport;
mod wrapper;

pub use codecs::{Codec, Codecs};
pub use events::{ConnectionState, Event, Events};
pub use transceiver_builder::TransceiverBuilder;
pub use wrapper::AsyncSdpSession;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActiveMediaId(u32);

impl ActiveMediaId {
    fn step(&mut self) -> Self {
        let id = *self;
        self.0 += 1;
        id
    }
}

slotmap::new_key_type! {
    pub struct LocalMediaId;
    pub struct TransportId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SocketId(TransportId, SocketUse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SocketUse {
    Rtp,
    Rtcp,
}

pub struct ReceiveData {
    pub transport_id: TransportId,
    pub source: SocketAddr,
    pub data: Vec<u8>,
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
    local_media: SlotMap<LocalMediaId, LocalMedia>,

    // Active media
    next_active_media_id: ActiveMediaId, // TODO: this will be used to set the `mid` and `msid?` when creating offers?
    state: Vec<MediaEntry>,

    // Transports
    transports: SlotMap<TransportId, Transport>,

    events: Events,
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

    fn active_mut(&mut self) -> Option<&mut ActiveMedia> {
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

    /// The RTP session for this media
    rtp_session: RtpSession,

    /// Optional mid, this is only one if both offer and answer have the mid attribute set
    mid: Option<BytesStr>,

    /// SDP Send/Recv direction
    direction: DirectionBools,

    /// Which transport is used by this media
    transport: TransportId,

    /// Which codec is negotiated
    codec_pt: u8,
    codec: Codec,
}

impl ActiveMedia {
    fn matches(
        &self,
        transports: &SlotMap<TransportId, Transport>,
        desc: &MediaDescription,
    ) -> bool {
        if self.media_type != desc.media.media_type {
            return false;
        }

        if let Some((self_mid, desc_mid)) = self.mid.as_ref().zip(desc.mid.as_ref()) {
            return self_mid == desc_mid;
        }

        transports[self.transport].remote_rtp_address.port() == desc.media.port
    }
}

impl SdpSession {
    pub fn new(address: IpAddr) -> Self {
        SdpSession {
            sdp_id: u64::from(rand::random::<u16>()),
            sdp_version: u64::from(rand::random::<u16>()),
            address,
            local_media: SlotMap::with_key(),
            next_active_media_id: ActiveMediaId(0),
            state: Vec::new(),
            transports: SlotMap::with_key(),
            events: Events::default(),
        }
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(
        &mut self,
        codecs: Codecs,
        limit: usize,
        direction: Direction,
    ) -> LocalMediaId {
        self.local_media.insert(LocalMedia {
            codecs,
            limit,
            use_count: 0,
            direction: direction.into(),
        })
    }

    pub fn remove_local_media(&mut self, local_media_id: LocalMediaId) {
        self.local_media.remove(local_media_id);
    }

    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    pub fn set_socket_port(&mut self, socket_id: SocketId, port: u16) {
        let transport = &mut self.transports[socket_id.0];

        match socket_id.1 {
            SocketUse::Rtp => transport.local_rtp_port = Some(port),
            SocketUse::Rtcp => transport.local_rtcp_port = Some(port),
        }
    }

    pub fn receive_sdp_offer(&mut self, offer: SessionDescription) -> Result<(), Error> {
        let mut new_state = vec![];

        for (m_line_index, remote_media_desc) in offer.media_descriptions.iter().enumerate() {
            let requested_direction: DirectionBools = remote_media_desc.direction.flipped().into();

            // First thing: Search the current state for an entry that matches this description - and update accordingly
            let matched_position = self.state.iter().position(|e| match e {
                MediaEntry::Active(active_media) => {
                    active_media.matches(&self.transports, remote_media_desc)
                }
                MediaEntry::Rejected(media_type) => {
                    *media_type == remote_media_desc.media.media_type
                }
            });

            if let Some(position) = matched_position {
                // Remove the entry and only if its active, don't consider trying to creating a new media session
                if let MediaEntry::Active(mut active_media) = self.state.remove(position) {
                    self.update_active_media(requested_direction, &mut active_media);
                    new_state.push(MediaEntry::Active(active_media));
                    continue;
                }
            }

            // Reject any invalid/inactive media descriptions
            if remote_media_desc.direction == Direction::Inactive
                || remote_media_desc.media.port == 0
            {
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            }

            // resolve remote rtp & rtcp address
            let (remote_rtp_address, remote_rtcp_address) =
                resolve_rtp_and_rtcp_address(&offer, remote_media_desc)?;

            // Choose local media for this media description
            let chosen_media = self.local_media.iter_mut().find_map(|(id, local_media)| {
                local_media
                    .maybe_use_for_offer(remote_media_desc)
                    .map(|config| (id, config))
            });

            let Some((local_media_id, (codec, codec_pt, negotiated_direction))) = chosen_media
            else {
                // no local media found for this
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            };

            let active_media_id = self.next_active_media_id.step();

            // Get or create transport for the m-line
            let transport = self.get_or_create_transport(
                &new_state,
                &offer,
                remote_media_desc,
                remote_rtp_address,
                remote_rtcp_address,
                active_media_id,
            )?;

            let Some(transport) = transport else {
                // No transport was found or created, reject media
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            };

            new_state.push(MediaEntry::Active(ActiveMedia {
                id: active_media_id,
                local_media_id,
                media_type: remote_media_desc.media.media_type,
                rtp_session: RtpSession::new(rand::random(), codec.clock_rate),
                mid: remote_media_desc.mid.clone(),
                direction: negotiated_direction,
                transport,
                codec_pt,
                codec,
            }));
        }

        // Store new state and destroy all media sessions
        let old_state = replace(&mut self.state, new_state);

        for entry in old_state {
            if let MediaEntry::Active(active) = entry {
                self.local_media[active.local_media_id].use_count -= 1;
            }
        }

        // Remove all transports that are not being used anymore
        self.transports.retain(|id, _| {
            self.state
                .iter()
                .filter_map(MediaEntry::active)
                .any(|active| active.transport == id)
        });

        Ok(())
    }

    fn update_active_media(
        &mut self,
        requested_direction: DirectionBools,
        active_media: &mut ActiveMedia,
    ) {
        let transport = &mut self.transports[active_media.transport];

        // // If remote wants to receive data, but we're not sending anything
        // if requested_direction.send && !active_media.direction.send {
        //     if let Some(create_sender) = &mut active_media.builder.create_sender {
        //         let (tx, rx) = mpsc::channel(8);
        //         transport.handle.set_sender(active_media.id, rx).await;
        //         create_sender(tx);
        //     }
        // }

        // // If remote is not receiving anything and we're sending, we need to stop sending
        // if !requested_direction.send
        //     && active_media.direction.send
        //     && active_media.builder.create_sender.is_some()
        // {
        //     transport.handle.remove_sender(active_media.id).await;
        // }

        // // If remote wants to send us something, but we're not receiving yet
        // if requested_direction.recv && !active_media.direction.recv {
        //     if let Some(create_receiver) = &mut active_media.builder.create_receiver {
        //         let (tx, rx) = mpsc::channel(8);
        //         transport.handle.set_receiver(active_media.id, tx).await;
        //         create_receiver(rx);
        //     }
        // }

        // // If remote is not sending anything and we're received, we need to stop receiving
        // if !requested_direction.recv
        //     && active_media.direction.recv
        //     && active_media.builder.create_receiver.is_some()
        // {
        //     transport.handle.remove_receiver(active_media.id).await;
        // }

        // active_media.direction.send =
        //     requested_direction.send && active_media.builder.create_sender.is_some();
        // active_media.direction.recv =
        //     requested_direction.recv && active_media.builder.create_receiver.is_some();
    }

    /// Get or create a transport for the given media description
    ///
    /// If the transport type is unknown or cannot be created Ok(None) is returned. The media section must then be declined.
    fn get_or_create_transport(
        &mut self,
        new_state: &[MediaEntry],
        offer: &SessionDescription,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
        active_media_id: ActiveMediaId,
    ) -> Result<Option<TransportId>, Error> {
        match remote_media_desc
            .mid
            .as_ref()
            .and_then(|mid| self.find_bundled_transport(new_state, offer, mid))
        {
            Some(id) => {
                // Bundled transport found, use that one
                Ok(Some(id))
            }

            None => {
                // Not bundled or no transport for the group created yet

                let Some(transport) = Transport::create_from_offer(
                    &mut self.events,
                    remote_media_desc,
                    remote_rtp_address,
                    remote_rtcp_address,
                    active_media_id,
                )?
                else {
                    return Ok(None);
                };

                let transport_id = self.transports.insert(transport);

                if remote_media_desc.rtcp_mux {
                    self.events.push(Event::CreateUdpSocketPair {
                        socket_ids: [
                            SocketId(transport_id, SocketUse::Rtp),
                            SocketId(transport_id, SocketUse::Rtcp),
                        ],
                    });
                } else {
                    self.events.push(Event::CreateUdpSocket {
                        socket_id: SocketId(transport_id, SocketUse::Rtp),
                    });
                }

                Ok(Some(transport_id))
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

                group.mids.contains(mid).then_some(m.transport)
            })
    }

    pub fn create_sdp_answer(&self) -> SessionDescription {
        let media = self.state.iter().map(|entry| {
            let active = match entry {
                MediaEntry::Active(active_media) => active_media,
                MediaEntry::Rejected(media_type) => return MediaDescription::rejected(*media_type),
            };

            let rtpmap = RtpMap {
                payload: active.codec_pt,
                encoding: active.codec.name.as_ref().into(),
                clock_rate: active.codec.clock_rate,
                params: Default::default(),
            };

            let fmtps = active.codec.params.iter().map(|param| Fmtp {
                format: active.codec_pt,
                params: param.as_str().into(),
            });

            let transport = &self.transports[active.transport];

            let mut media_desc = MediaDescription {
                media: Media {
                    media_type: active.media_type,
                    port: transport
                        .local_rtp_port
                        .expect("Did not set port for RTP socket"),
                    ports_num: None,
                    proto: transport.sdp_type(),
                    fmts: vec![active.codec_pt],
                },
                connection: None,
                bandwidth: vec![],
                direction: active.direction.into(),
                rtcp: transport.local_rtcp_port.map(|port| Rtcp {
                    port,
                    address: None,
                }),
                rtcp_mux: transport.remote_rtp_address == transport.remote_rtcp_address,
                mid: active.mid.clone(),
                rtpmap: vec![rtpmap],
                fmtp: fmtps.collect(),
                ice_ufrag: None,
                ice_pwd: None,
                ice_candidates: vec![],
                ice_end_of_candidates: false,
                crypto: vec![],
                extmap: transport.extension_ids.to_extmap(),
                extmap_allow_mixed: false,
                ssrc: vec![],
                setup: None,
                fingerprint: vec![],
                attributes: vec![],
            };

            transport.populate_offer(&mut media_desc);

            media_desc
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
            origin: Origin {
                username: "-".into(),
                session_id: self.sdp_id.to_string().into(),
                session_version: self.sdp_version.to_string().into(),
                address: self.address.into(),
            },
            name: "-".into(),
            connection: Some(Connection {
                address: self.address.into(),
                ttl: None,
                num: None,
            }),
            bandwidth: vec![],
            time: Time { start: 0, stop: 0 },
            direction: Direction::SendRecv,
            group: group.collect(),
            extmap: vec![],
            extmap_allow_mixed: true,
            ice_lite: false,
            ice_options: IceOptions::default(),
            ice_ufrag: None,
            ice_pwd: None,
            setup: None,
            fingerprint: vec![],
            attributes: vec![],
            media_descriptions: media.collect(),
        }
    }

    /// Returns a duration after which [`poll`](Self::poll) must be called
    pub fn timeout(&self) -> Option<Duration> {
        let mut timeout = None;

        for transport in self.transports.values() {
            match (&mut timeout, transport.timeout()) {
                (None, Some(new)) => timeout = Some(new),
                (Some(prev), Some(new)) => *prev = min(*prev, new),
                _ => {}
            }
        }

        for media in self.state.iter().filter_map(MediaEntry::active) {
            match (&mut timeout, media.rtp_session.pop_rtp_after(None)) {
                (None, Some(new)) => timeout = Some(new),
                (Some(prev), Some(new)) => *prev = min(*prev, new),
                _ => {}
            }
        }

        timeout
    }

    /// Poll for new events. Call [`pop_event`](Self::pop_event) to handle them.
    pub fn poll(&mut self) {
        for (transport_id, transport) in &mut self.transports {
            transport.poll(transport_id, &mut self.events);
        }

        for media in self.state.iter_mut().filter_map(MediaEntry::active_mut) {
            if let Some(rtp_packet) = media.rtp_session.pop_rtp(None) {
                self.events.push(Event::ReceiveRTP {
                    media_id: media.id,
                    packet: rtp_packet,
                });
            }
        }
    }

    pub fn receive(&mut self, socket_id: SocketId, data: &mut Cow<[u8]>, source: SocketAddr) {
        let transport = &mut self.transports[socket_id.0];

        match transport.receive(&mut self.events, data, source, socket_id) {
            ReceivedPacket::Rtp => {
                let rtp_packet = match RtpPacket::parse(data) {
                    Ok(rtp_packet) => rtp_packet,
                    Err(e) => {
                        log::debug!("Failed to parse RTP packet, {e:?}");
                        return;
                    }
                };

                let packet = rtp_packet.get();
                let extensions = RtpExtensions::from_packet(&transport.extension_ids, &packet);

                // Find the matching media using the mid field
                let entry = self
                    .state
                    .iter_mut()
                    .filter_map(MediaEntry::active_mut)
                    .filter(|m| m.transport == socket_id.0)
                    .find(|e| match (&e.mid, extensions.mid) {
                        (Some(a), Some(b)) => a.as_bytes() == b,
                        _ => false,
                    });

                // Try to find the correct media using the payload type
                let entry = if let Some(entry) = entry {
                    Some(entry)
                } else {
                    self.state
                        .iter_mut()
                        .filter_map(MediaEntry::active_mut)
                        .filter(|m| m.transport == socket_id.0)
                        .find(|e| e.codec_pt == packet.payload_type())
                };

                if let Some(entry) = entry {
                    entry.rtp_session.recv_rtp(rtp_packet);
                } else {
                    log::warn!("Failed to find media for RTP packet ssrc={}", packet.ssrc());
                }
            }
            ReceivedPacket::Rtcp => {
                let rtcp_compound = match Compound::parse(data) {
                    Ok(rtcp_compound) => rtcp_compound,
                    Err(e) => {
                        log::debug!("Failed to parse incoming RTCP packet, {e}");
                        return;
                    }
                };
            }
            ReceivedPacket::TransportSpecific => {
                // ignore
            }
        }
    }

    pub fn send_rtp(&mut self, media_id: ActiveMediaId, mut packet: RtpPacket) {
        let media = self
            .state
            .iter_mut()
            .filter_map(MediaEntry::active_mut)
            .find(|m| m.id == media_id)
            .unwrap();
        let transport = &mut self.transports[media.transport];

        // Tell the RTP session that a packet is being sent
        media.rtp_session.send_rtp(&packet);

        // Re-serialize the packet with the extensions set
        let mut packet_mut = packet.get_mut();
        packet_mut.set_ssrc(media.rtp_session.ssrc());

        let extensions = RtpExtensions {
            mid: media.mid.as_ref().map(|e| e.as_bytes()),
        };
        let builder = extensions.write(&transport.extension_ids, rtp::to_builder(&packet_mut));
        let mut writer = rtp::RtpPacketWriterVec::default();
        let mut packet = builder.write(&mut writer).unwrap();

        // Use the transport to maybe protect the packet
        transport.protect_rtp(&mut packet);

        self.events.push(Event::SendData {
            socket: SocketId(media.transport, SocketUse::Rtp),
            data: packet,
            target: transport.remote_rtp_address,
        });
    }

    fn send_rtcp(&mut self, media_id: ActiveMediaId) {
        let media = self
            .state
            .iter_mut()
            .filter_map(MediaEntry::active_mut)
            .find(|m| m.id == media_id)
            .unwrap();

        let mut encode_buf = vec![0u8; 65535];

        let len = match media.rtp_session.write_rtcp_report(&mut encode_buf) {
            Ok(len) => len,
            Err(e) => {
                log::warn!("Failed to write RTCP packet, {e:?}");
                return;
            }
        };

        encode_buf.truncate(len);

        let transport = &mut self.transports[media.transport];

        transport.protect_rtcp(&mut encode_buf);

        let socket_use = if transport.local_rtcp_port.is_some() {
            SocketUse::Rtp
        } else {
            SocketUse::Rtcp
        };

        self.events.push(Event::SendData {
            socket: SocketId(media.transport, socket_use),
            data: encode_buf,
            target: transport.remote_rtcp_address,
        });
    }
}

fn resolve_rtp_and_rtcp_address(
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

    let remote_rtp_address = resolve_tagged_address(&remote_rtp_address, remote_rtp_port)?;
    let remote_rtcp_address = resolve_tagged_address(&remote_rtcp_address, remote_rtcp_port)?;

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

fn resolve_tagged_address(address: &TaggedAddress, port: u16) -> io::Result<SocketAddr> {
    // TODO: do not resolve here directly
    match address {
        TaggedAddress::IP4(ipv4_addr) => Ok(SocketAddr::from((*ipv4_addr, port))),
        TaggedAddress::IP4FQDN(bytes_str) => (bytes_str.as_str(), port)
            .to_socket_addrs()?
            .find(SocketAddr::is_ipv4)
            .ok_or_else(|| {
                io::Error::other(format!("Failed to find IPv4 address for {bytes_str}"))
            }),
        TaggedAddress::IP6(ipv6_addr) => Ok(SocketAddr::from((*ipv6_addr, port))),
        TaggedAddress::IP6FQDN(bytes_str) => (bytes_str.as_str(), port)
            .to_socket_addrs()?
            .find(SocketAddr::is_ipv6)
            .ok_or_else(|| {
                io::Error::other(format!("Failed to find IPv6 address for {bytes_str}"))
            }),
    }
}

// i'm too lazy to work with the direction type, so using this as a cop out
#[derive(Debug, Clone, Copy)]
struct DirectionBools {
    send: bool,
    recv: bool,
}

impl From<DirectionBools> for Direction {
    fn from(value: DirectionBools) -> Self {
        match (value.send, value.recv) {
            (true, true) => Direction::SendRecv,
            (true, false) => Direction::SendOnly,
            (false, true) => Direction::RecvOnly,
            (false, false) => Direction::Inactive,
        }
    }
}

impl From<Direction> for DirectionBools {
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
