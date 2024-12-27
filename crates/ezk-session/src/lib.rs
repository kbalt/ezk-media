use bytesstr::BytesStr;
use dtls_srtp::{DtlsSetup, DtlsSrtpSession};
use ezk_rtp::{RtpPacket, RtpSession};
use local_media::LocalMedia;
use sdp_types::{
    Connection, Direction, ExtMap, Fingerprint, Fmtp, Group, IceOptions, Media, MediaDescription,
    MediaType, Origin, Rtcp, RtpMap, SessionDescription, Setup, SrtpCrypto, TaggedAddress, Time,
    TransportProtocol,
};
use slotmap::SlotMap;
use std::{
    collections::{HashMap, VecDeque},
    io,
    mem::replace,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
};
use transport::IdentifyableBy;

mod codecs;
mod dtls_srtp;
mod local_media;
mod sdes_srtp;
mod transceiver_builder;
mod transport;
mod wrapper;

pub use codecs::{Codec, Codecs};
pub use transceiver_builder::TransceiverBuilder;
pub use wrapper::SdpSession as AsyncSdpSession;

// TODO: have this not be hardcoded
const RTP_MID_HDREXT_ID: u8 = 1;
const RTP_MID_HDREXT: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

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

pub enum Instruction {
    /// Create 2 UdpSockets for a media session
    ///
    /// This is called when rtcp-mux is not available
    CreateUdpSocketPair { socket_ids: [SocketId; 2] },

    /// Create a single UdpSocket for a media session
    CreateUdpSocket { socket_id: SocketId },

    /// Send data
    SendData {
        socket: SocketId,
        data: Vec<u8>,
        target: SocketAddr,
    },

    /// Receive RTP
    ReceiveRTP { packet: RtpPacket },

    /// Added a track
    TrackAdded {},
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

    instructions: VecDeque<Instruction>,
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

struct Transport {
    local_rtp_port: Option<u16>,
    local_rtcp_port: Option<u16>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: SocketAddr,

    kind: TransportKind,

    mid_rtp_id: Option<u8>,
}

struct Entry {
    rtp_session: RtpSession,
    remote_identifyable_by: IdentifyableBy,
}

enum TransportKind {
    Rtp,
    SdesSrtp {
        crypto: Vec<SrtpCrypto>,
        inbound: srtp::Session,
        outbound: srtp::Session,
    },
    DtlsSrtp {
        fingerprint: Vec<Fingerprint>,
        setup: Setup,

        dtls: DtlsSrtpSession,
        srtp: Option<(srtp::Session, srtp::Session)>,
    },
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
            instructions: VecDeque::new(),
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

    pub fn pop_instruction(&mut self) -> Option<Instruction> {
        self.instructions.pop_front()
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
                // Remove the entry and only if its active, we don't consider creating a new media session
                if let MediaEntry::Active(mut active_media) = self.state.remove(position) {
                    self.update_active_media(requested_direction, &mut active_media);
                    new_state.push(MediaEntry::Active(active_media));
                    continue;
                }
            }

            // Reject any invalid/inactive m-lines
            if remote_media_desc.direction == Direction::Inactive
                || remote_media_desc.media.port == 0
            {
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            }

            // resolve remote rtp & rtcp address
            let (remote_rtp_address, remote_rtcp_address) =
                resolve_rtp_and_rtcp_address(&offer, remote_media_desc)?;

            // Choose local media for this m-line
            let Some((local_media_id, (codec, codec_pt, negotiated_direction))) =
                self.local_media.iter_mut().find_map(|(id, local_media)| {
                    let config = local_media.maybe_use_for_offer(remote_media_desc)?;

                    Some((id, config))
                })
            else {
                // no local media found for this
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            };

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
                new_state.push(MediaEntry::Rejected(remote_media_desc.media.media_type));
                continue;
            };

            let active_media_id = self.next_active_media_id.step();

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
                let mid_rtp_id = remote_media_desc
                    .extmap
                    .iter()
                    .find(|extmap| extmap.uri == RTP_MID_HDREXT)
                    .map(|extmap| extmap.id);

                let kind = match remote_media_desc.media.proto {
                    TransportProtocol::RtpAvp => TransportKind::Rtp,
                    TransportProtocol::RtpSavp => {
                        let (crypto, inbound, outbound) =
                            sdes_srtp::negotiate_sdes_srtp(&remote_media_desc.crypto)?;

                        TransportKind::SdesSrtp {
                            crypto,
                            inbound,
                            outbound,
                        }
                    }
                    TransportProtocol::UdpTlsRtpSavp => {
                        let setup = match remote_media_desc.setup {
                            Some(Setup::Active) => DtlsSetup::Accept,
                            Some(Setup::Passive) => DtlsSetup::Connect,
                            Some(Setup::ActPass) => {
                                // Use passive when accepting an offer so both sides will have the DTLS fingerprint
                                // before any request is sent
                                DtlsSetup::Accept
                            }
                            Some(Setup::HoldConn) | None => {
                                // Missing or invalid setup specified, just silently fail here
                                return Ok(None);
                            }
                        };

                        let remote_fingerprints: Vec<_> = remote_media_desc
                            .fingerprint
                            .iter()
                            .filter_map(|e| {
                                Some((
                                    dtls_srtp::to_openssl_digest(&e.algorithm)?,
                                    e.fingerprint.clone(),
                                ))
                            })
                            .collect();

                        let dtls = DtlsSrtpSession::new(remote_fingerprints, setup)?;

                        TransportKind::DtlsSrtp {
                            fingerprint: vec![dtls.fingerprint()],
                            setup: match setup {
                                DtlsSetup::Accept => Setup::Passive,
                                DtlsSetup::Connect => Setup::Active,
                            },
                            dtls,
                            srtp: None,
                        }
                    }
                    _ => return Ok(None),
                };

                let transport_id = self.transports.insert(Transport {
                    local_rtp_port: None,
                    local_rtcp_port: None,
                    remote_rtp_address,
                    remote_rtcp_address,
                    kind,
                    mid_rtp_id,
                });

                if remote_media_desc.rtcp_mux {
                    self.instructions
                        .push_back(Instruction::CreateUdpSocketPair {
                            socket_ids: [
                                SocketId(transport_id, SocketUse::Rtp),
                                SocketId(transport_id, SocketUse::Rtcp),
                            ],
                        });
                } else {
                    self.instructions.push_back(Instruction::CreateUdpSocket {
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

            let mut extmap = vec![];
            if active.mid.is_some() {
                extmap.push(ExtMap {
                    id: RTP_MID_HDREXT_ID,
                    uri: BytesStr::from_static(RTP_MID_HDREXT),
                    direction: Direction::SendRecv,
                });
            }

            let transport = &self.transports[active.transport];

            let mut fingerprint = vec![];
            let mut crypto = vec![];
            let mut setup = None;

            let proto = match &transport.kind {
                TransportKind::Rtp => TransportProtocol::RtpAvp,
                TransportKind::SdesSrtp { crypto: c, .. } => {
                    crypto.extend_from_slice(c);
                    TransportProtocol::RtpSavp
                }
                TransportKind::DtlsSrtp {
                    fingerprint: f,
                    setup: s,
                    ..
                } => {
                    fingerprint.extend_from_slice(f);
                    setup = Some(*s);
                    TransportProtocol::UdpTlsRtpSavp
                }
            };

            MediaDescription {
                media: Media {
                    media_type: active.media_type,
                    port: transport.local_rtp_port.unwrap(),
                    ports_num: None,
                    proto,
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
                crypto,
                extmap,
                extmap_allow_mixed: false,
                ssrc: vec![],
                setup,
                fingerprint,
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
