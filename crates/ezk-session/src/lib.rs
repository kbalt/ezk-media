#![warn(unreachable_pub)]

use bytesstr::BytesStr;
use ezk_rtp::{rtcp_types::Compound, RtpPacket, RtpSession};
use local_media::LocalMedia;
use rtp::RtpExtensions;
use sdp_types::{
    Connection, Direction, Fmtp, Group, IceOptions, Media, MediaDescription, MediaType, Origin,
    Rtcp, RtpMap, SessionDescription, TaggedAddress, Time,
};
use slotmap::SlotMap;
use std::{
    borrow::Cow,
    cmp::min,
    collections::HashMap,
    io,
    mem::replace,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use transport::{
    NewTransport, ReceivedPacket, SessionTransportState, SocketUse, Transport, TransportBuilder,
    TransportEvent,
};

mod codecs;
mod events;
mod local_media;
mod options;
mod rtp;
mod transport;
mod wrapper;

pub use codecs::{Codec, Codecs};
pub use events::{ConnectionState, Event, Events};
pub use options::{BundlePolicy, Options, RtcpMuxPolicy, TransportType};
pub use wrapper::AsyncSdpSession;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaId(u32);

impl MediaId {
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
pub struct SocketId(pub TransportId, pub SocketUse);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct SdpSession {
    options: Options,

    id: u64,
    version: u64,

    // Local ip address to use
    address: IpAddr,

    /// State shared between transports
    transport_state: SessionTransportState,

    // Local configured media
    local_media: SlotMap<LocalMediaId, LocalMedia>,

    /// Counter for local media ids
    next_media_id: MediaId,
    /// List of all media, representing the current state
    state: Vec<ActiveMedia>,

    // Transports
    transports: SlotMap<TransportId, TransportEntry>,

    /// Pending changes which will be (maybe partially) applied once the offer/answer exchange has been completed
    pending: Vec<PendingChange>,

    events: Events,
}

enum TransportEntry {
    Transport(Transport),
    TransportBuilder(TransportBuilder),
}

impl TransportEntry {
    fn type_(&self) -> TransportType {
        match self {
            TransportEntry::Transport(transport) => transport.type_(),
            TransportEntry::TransportBuilder(transport_builder) => transport_builder.type_(),
        }
    }

    #[track_caller]
    fn unwrap(&self) -> &Transport {
        match self {
            TransportEntry::Transport(transport) => transport,
            TransportEntry::TransportBuilder(..) => {
                panic!("Tried to access incomplete transport")
            }
        }
    }

    #[track_caller]
    fn unwrap_mut(&mut self) -> &mut Transport {
        match self {
            TransportEntry::Transport(transport) => transport,
            TransportEntry::TransportBuilder(..) => {
                panic!("Tried to access incomplete transport")
            }
        }
    }

    fn filter(&self) -> Option<&Transport> {
        match self {
            TransportEntry::Transport(transport) => Some(transport),
            TransportEntry::TransportBuilder(..) => None,
        }
    }
}

struct ActiveMedia {
    id: MediaId,
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
        transports: &SlotMap<TransportId, TransportEntry>,
        desc: &MediaDescription,
    ) -> bool {
        if self.media_type != desc.media.media_type {
            return false;
        }

        if let Some((self_mid, desc_mid)) = self.mid.as_ref().zip(desc.mid.as_ref()) {
            return self_mid == desc_mid;
        }

        if let TransportEntry::Transport(transport) = &transports[self.transport] {
            transport.remote_rtp_address.port() == desc.media.port
        } else {
            false
        }
    }
}

enum PendingChange {
    AddMedia(PendingMedia),
    RemoveMedia(MediaId),
    ChangeDirection(MediaId, Direction),
}

struct PendingMedia {
    id: MediaId,
    local_media_id: LocalMediaId,
    media_type: MediaType,
    mid: String,
    direction: DirectionBools,
    /// Transport to use when not bundling
    standalone_transport: Option<TransportId>,
    /// Transport to use when bundling
    bundle_transport: TransportId,
}

pub struct SdpAnswerState(Vec<SdpResponseEntry>);

/// Helper type to record the state of the SdpAnswer
enum SdpResponseEntry {
    Active(MediaId),
    Rejected {
        media_type: MediaType,
        mid: Option<BytesStr>,
    },
}

impl SdpSession {
    pub fn new(address: IpAddr, options: Options) -> Self {
        SdpSession {
            options,
            id: u64::from(rand::random::<u16>()),
            version: u64::from(rand::random::<u16>()),
            address,
            transport_state: SessionTransportState::default(),
            local_media: SlotMap::with_key(),
            next_media_id: MediaId(0),
            state: Vec::new(),
            pending: Vec::new(),
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

    pub fn add_media(&mut self, local_media_id: LocalMediaId, direction: Direction) -> MediaId {
        let media_id = self.next_media_id.step();

        let (standalone_transport, bundle_transport) = match self.options.bundle_policy {
            BundlePolicy::Balanced => unimplemented!(),
            BundlePolicy::MaxCompat => {
                unimplemented!()
            }
            BundlePolicy::MaxBundle => {
                // Force bundling, only create a transport if none of the type we should be offering exists

                // TODO: if a "better" transport already exists use that instead?
                let transport = self
                    .transports
                    .iter()
                    .find(|(_, t)| t.type_() == self.options.offer_transport);

                let transport_id = if let Some((existing_transport, _)) = transport {
                    existing_transport
                } else {
                    self.transports
                        .insert(TransportEntry::TransportBuilder(TransportBuilder::new(
                            &mut self.transport_state,
                            self.options.offer_transport,
                        )))
                };

                (None, transport_id)
            }
        };

        self.pending.push(PendingChange::AddMedia(PendingMedia {
            id: media_id,
            local_media_id,
            media_type: self.local_media[local_media_id].codecs.media_type,
            mid: media_id.0.to_string(),
            direction: direction.into(),
            standalone_transport,
            bundle_transport,
        }));

        media_id
    }

    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    pub fn receive_sdp_offer(
        &mut self,
        offer: SessionDescription,
    ) -> Result<SdpAnswerState, Error> {
        let mut new_state = vec![];
        let mut response = vec![];

        for remote_media_desc in &offer.media_descriptions {
            let requested_direction: DirectionBools = remote_media_desc.direction.flipped().into();

            // First thing: Search the current state for an entry that matches this description - and update accordingly
            let matched_position = self
                .state
                .iter()
                .position(|media| media.matches(&self.transports, remote_media_desc));

            if let Some(position) = matched_position {
                let mut media = self.state.remove(position);
                self.update_media(requested_direction, &mut media);
                response.push(SdpResponseEntry::Active(media.id));
                new_state.push(media);
                continue;
            }

            // Reject any invalid/inactive media descriptions
            if remote_media_desc.direction == Direction::Inactive
                || remote_media_desc.media.port == 0
            {
                response.push(SdpResponseEntry::Rejected {
                    media_type: remote_media_desc.media.media_type,
                    mid: remote_media_desc.mid.clone(),
                });
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
                response.push(SdpResponseEntry::Rejected {
                    media_type: remote_media_desc.media.media_type,
                    mid: remote_media_desc.mid.clone(),
                });
                continue;
            };

            let media_id = self.next_media_id.step();

            // Get or create transport for the m-line
            let transport = self.get_or_create_transport(
                &new_state,
                &offer,
                remote_media_desc,
                remote_rtp_address,
                remote_rtcp_address,
            )?;

            let Some(transport) = transport else {
                // No transport was found or created, reject media
                response.push(SdpResponseEntry::Rejected {
                    media_type: remote_media_desc.media.media_type,
                    mid: remote_media_desc.mid.clone(),
                });
                continue;
            };

            response.push(SdpResponseEntry::Active(media_id));
            new_state.push(ActiveMedia {
                id: media_id,
                local_media_id,
                media_type: remote_media_desc.media.media_type,
                rtp_session: RtpSession::new(rand::random(), codec.clock_rate),
                mid: remote_media_desc.mid.clone(),
                direction: negotiated_direction,
                transport,
                codec_pt,
                codec,
            });
        }

        // Store new state and destroy all media sessions
        let remove_media = replace(&mut self.state, new_state);

        for media in remove_media {
            self.local_media[media.local_media_id].use_count -= 1;
        }

        // Remove all transports that are not being used anymore
        self.transports.retain(|id, _| {
            // Is the transport in use by active media?
            let in_use_by_active = self.state.iter().any(|media| media.transport == id);

            // Is the transport in use by any pending changes?
            let in_use_by_pending = self.pending.iter().any(|change| {
                if let PendingChange::AddMedia(add_media) = change {
                    add_media.bundle_transport == id || add_media.standalone_transport == Some(id)
                } else {
                    false
                }
            });

            in_use_by_active || in_use_by_pending
        });

        Ok(SdpAnswerState(response))
    }

    fn update_media(&mut self, requested_direction: DirectionBools, media: &mut ActiveMedia) {
        // let transport = self.transports[media.transport].unwrap_mut();

        if media.direction != requested_direction {
            // todo: emit direction change event
        }
    }

    /// Get or create a transport for the given media description
    ///
    /// If the transport type is unknown or cannot be created Ok(None) is returned. The media section must then be declined.
    fn get_or_create_transport(
        &mut self,
        new_state: &[ActiveMedia],
        offer: &SessionDescription,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Option<TransportId>, Error> {
        // See if there's a transport to be reused via BUNDLE group
        if let Some(id) = remote_media_desc
            .mid
            .as_ref()
            .and_then(|mid| self.find_bundled_transport(new_state, offer, mid))
        {
            return Ok(Some(id));
        }

        let transport = Transport::create_from_offer(
            &mut self.transport_state,
            remote_media_desc,
            remote_rtp_address,
            remote_rtcp_address,
        )?;

        if let Some(transport) = transport {
            let transport_id = self.transports.insert(TransportEntry::Transport(transport));

            Ok(Some(transport_id))
        } else {
            Ok(None)
        }
    }

    fn find_bundled_transport(
        &self,
        new_state: &[ActiveMedia],
        offer: &SessionDescription,
        mid: &BytesStr,
    ) -> Option<TransportId> {
        let group = offer
            .group
            .iter()
            .find(|g| g.typ == "BUNDLE" && g.mids.contains(mid))?;

        new_state.iter().chain(&self.state).find_map(|m| {
            let mid = m.mid.as_ref()?;

            group.mids.contains(mid).then_some(m.transport)
        })
    }

    /// Create an SDP Answer from a given state, which must be created by a previous call to [`SdpSession::receive_sdp_offer`].
    ///
    /// # Panics
    ///
    /// This function will panic if any transport has not been assigned a port.
    pub fn create_sdp_answer(&self, state: SdpAnswerState) -> SessionDescription {
        let mut media_descriptions = vec![];

        for entry in state.0 {
            let active = match entry {
                SdpResponseEntry::Active(media_id) => self
                    .state
                    .iter()
                    .find(|media| media.id == media_id)
                    .unwrap(),
                SdpResponseEntry::Rejected { media_type, mid } => {
                    let mut desc = MediaDescription::rejected(media_type);
                    desc.mid = mid;
                    media_descriptions.push(desc);
                    continue;
                }
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

            let transport = self.transports[active.transport].unwrap();

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

            media_descriptions.push(media_desc);
        }

        // Create bundle group attributes
        let group = {
            let mut bundle_groups: HashMap<TransportId, Vec<BytesStr>> = HashMap::new();

            for media in self.state.iter() {
                if let Some(mid) = media.mid.clone() {
                    bundle_groups.entry(media.transport).or_default().push(mid);
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
                session_id: self.id.to_string().into(),
                session_version: self.version.to_string().into(),
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
            media_descriptions,
        }
    }

    /// Returns an iterator over all transports that have not yet had their port assigned to
    pub fn new_transports(&mut self) -> impl Iterator<Item = NewTransport<'_>> {
        self.transports
            .iter_mut()
            .map(|(id, t)| match t {
                TransportEntry::Transport(transport) => transport.as_new_transport(id),
                TransportEntry::TransportBuilder(transport_builder) => {
                    transport_builder.as_new_transport(id)
                }
            })
            .filter(|nt| {
                if nt.rtcp_mux {
                    nt.rtp_port.is_none()
                } else {
                    nt.rtp_port.is_none() || nt.rtcp_port.is_none()
                }
            })
    }

    /// Returns a duration after which [`poll`](Self::poll) must be called
    pub fn timeout(&self) -> Option<Duration> {
        let mut timeout = None;

        for transport in self.transports.values().filter_map(TransportEntry::filter) {
            match (&mut timeout, transport.timeout()) {
                (None, Some(new)) => timeout = Some(new),
                (Some(prev), Some(new)) => *prev = min(*prev, new),
                _ => {}
            }
        }

        for media in self.state.iter() {
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
            let TransportEntry::Transport(transport) = transport else {
                continue;
            };

            transport.poll();

            Self::propagate_transport_events(
                &self.state,
                &mut self.events,
                transport_id,
                transport,
            );
        }

        for media in self.state.iter_mut() {
            if let Some(rtp_packet) = media.rtp_session.pop_rtp(None) {
                self.events.push(Event::ReceiveRTP {
                    media_id: media.id,
                    packet: rtp_packet,
                });
            }
        }
    }

    /// "Converts" the the transport's event to the session events
    fn propagate_transport_events(
        state: &[ActiveMedia],
        events: &mut Events,
        transport_id: TransportId,
        transport: &mut Transport,
    ) {
        while let Some(event) = transport.pop_event() {
            match event {
                TransportEvent::ConnectionState { old, new } => {
                    // Emit the connection state event for every track that uses the transport
                    for media in state.iter().filter(|m| m.transport == transport_id) {
                        events.push(Event::ConnectionState {
                            media_id: media.id,
                            old,
                            new,
                        });
                    }
                }
                TransportEvent::SendData {
                    socket,
                    data,
                    target,
                } => {
                    events.push(Event::SendData {
                        socket: SocketId(transport_id, socket),
                        data,
                        target,
                    });
                }
            }
        }
    }

    pub fn receive(&mut self, socket_id: SocketId, data: &mut Cow<[u8]>, source: SocketAddr) {
        let transport = &mut self.transports[socket_id.0];

        let transport = match transport {
            TransportEntry::Transport(transport) => transport,
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.receive(data.to_vec(), source, socket_id.1);
                return;
            }
        };

        match transport.receive(data, source, socket_id.1) {
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

    pub fn send_rtp(&mut self, media_id: MediaId, mut packet: RtpPacket) {
        let media = self.state.iter_mut().find(|m| m.id == media_id).unwrap();
        let transport = self.transports[media.transport].unwrap_mut();

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

    fn send_rtcp(&mut self, media_id: MediaId) {
        let media = self.state.iter_mut().find(|m| m.id == media_id).unwrap();

        let mut encode_buf = vec![0u8; 65535];

        let len = match media.rtp_session.write_rtcp_report(&mut encode_buf) {
            Ok(len) => len,
            Err(e) => {
                log::warn!("Failed to write RTCP packet, {e:?}");
                return;
            }
        };

        encode_buf.truncate(len);

        let transport = self.transports[media.transport].unwrap_mut();

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
#[derive(Debug, Clone, Copy, PartialEq)]
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
