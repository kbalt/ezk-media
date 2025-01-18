#![warn(unreachable_pub)]

use bytesstr::BytesStr;
use events::{TransportChange, TransportRequiredChanges};
use ezk_ice::{IceAgent, ReceivedPkt, SocketUse};
use ezk_rtp::{rtcp_types::Compound, RtpPacket, RtpSession};
use local_media::LocalMedia;
use rtp::RtpExtensions;
use sdp_types::{
    Connection, Direction, Fmtp, Group, IceOptions, IcePassword, IceUsernameFragment, Media,
    MediaDescription, MediaType, Origin, Rtcp, RtpMap, SessionDescription, Time,
};
use slotmap::SlotMap;
use std::{
    cmp::min,
    collections::HashMap,
    io,
    mem::replace,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use transport::{
    ReceivedPacket, SessionTransportState, Transport, TransportBuilder, TransportEvent,
};

mod async_wrapper;
mod codecs;
mod events;
mod local_media;
mod options;
mod rtp;
mod transport;
mod wrapper;

pub use async_wrapper::AsyncSdpSession;
pub use codecs::{Codec, Codecs};
pub use events::{ConnectionState, Event, Events};
pub use options::{BundlePolicy, Options, RtcpMuxPolicy, TransportType};

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

    // Local configured media codecs
    next_pt: u8,
    local_media: SlotMap<LocalMediaId, LocalMedia>,

    /// Counter for local media ids
    next_media_id: MediaId,
    /// List of all media, representing the current state
    state: Vec<ActiveMedia>,

    // Transports
    transports: SlotMap<TransportId, TransportEntry>,

    /// Pending changes which will be (maybe partially) applied once the offer/answer exchange has been completed
    pending_changes: Vec<PendingChange>,
    transport_changes: Vec<TransportChange>,
    events: Events,
}

#[allow(clippy::large_enum_variant)]
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

    fn populate_desc(&self, desc: &mut MediaDescription) {
        match self {
            TransportEntry::Transport(transport) => transport.populate_desc(desc),
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.populate_desc(desc);
            }
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

    fn ports(&self) -> (Option<u16>, Option<u16>) {
        match self {
            TransportEntry::Transport(transport) => {
                (transport.local_rtp_port, transport.local_rtcp_port)
            }
            TransportEntry::TransportBuilder(transport_builder) => (
                transport_builder.local_rtp_port,
                transport_builder.local_rtcp_port,
            ),
        }
    }

    fn ice_agent(&self) -> Option<&IceAgent> {
        match self {
            TransportEntry::Transport(transport) => transport.ice_agent.as_ref(),
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.ice_agent.as_ref()
            }
        }
    }

    fn ice_agent_mut(&mut self) -> Option<&mut IceAgent> {
        match self {
            TransportEntry::Transport(transport) => transport.ice_agent.as_mut(),
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.ice_agent.as_mut()
            }
        }
    }
}

struct ActiveMedia {
    id: MediaId,
    local_media_id: LocalMediaId,

    media_type: MediaType,

    /// The RTP session for this media
    rtp_session: RtpSession,

    /// When to send the next RTCP report
    // TODO: do not start rtcp transmitting until transport is ready
    next_rtcp: Instant,

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

impl PendingChange {
    fn remove_media(&self) -> Option<MediaId> {
        match self {
            PendingChange::RemoveMedia(media_id) => Some(*media_id),
            _ => None,
        }
    }
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

impl PendingMedia {
    fn matches_answer(
        &self,
        transports: &SlotMap<TransportId, TransportEntry>,
        desc: &MediaDescription,
    ) -> bool {
        if self.media_type != desc.media.media_type {
            return false;
        }

        if let Some(answer_mid) = &desc.mid {
            return self.mid == answer_mid.as_str();
        }

        if let Some(standalone_transport) = self.standalone_transport {
            if transports[standalone_transport].type_().sdp_type() == desc.media.proto {
                return true;
            }
        }

        transports[self.bundle_transport].type_().sdp_type() == desc.media.proto
    }
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
            next_pt: 96,
            local_media: SlotMap::with_key(),
            next_media_id: MediaId(0),
            state: Vec::new(),
            transports: SlotMap::with_key(),
            pending_changes: Vec::new(),
            transport_changes: Vec::new(),
            events: Events::default(),
        }
    }

    /// Add a stun server to use for ICE
    pub fn add_stun_server(&mut self, server: SocketAddr) {
        self.transport_state.add_stun_server(server);

        for transport in self.transports.values_mut() {
            match transport {
                TransportEntry::Transport(transport) => {
                    if let Some(ice_agent) = &mut transport.ice_agent {
                        ice_agent.add_stun_server(server);
                    }
                }
                TransportEntry::TransportBuilder(transport_builder) => {
                    if let Some(ice_agent) = &mut transport_builder.ice_agent {
                        ice_agent.add_stun_server(server);
                    }
                }
            }
        }
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(
        &mut self,
        mut codecs: Codecs,
        limit: u32,
        direction: Direction,
    ) -> LocalMediaId {
        // Assign dynamic payload type numbers
        for codec in &mut codecs.codecs {
            if codec.pt.is_some() {
                continue;
            }

            codec.pt = Some(self.next_pt);

            self.next_pt += 1;

            if self.next_pt > 127 {
                todo!("implement error when overflowing the payload type")
            }
        }

        self.local_media.insert(LocalMedia {
            codecs,
            limit,
            use_count: 0,
            direction: direction.into(),
        })
    }

    /// Request a new media session to be created
    pub fn add_media(&mut self, local_media_id: LocalMediaId, direction: Direction) -> MediaId {
        let media_id = self.next_media_id.step();

        // Find out which type of transport to use for this media
        let transport_type = self
            .transports
            .values()
            .map(|t| t.type_())
            .max()
            .unwrap_or(self.options.offer_transport);

        // Find a transport of the previously found type to bundle
        let bundle_transport_id = self
            .transports
            .iter()
            .find(|(_, t)| t.type_() == transport_type)
            .map(|(id, _)| id);

        let (standalone_transport, bundle_transport) = match self.options.bundle_policy {
            BundlePolicy::MaxCompat => {
                let standalone_transport_id = self.transports.insert_with_key(|id| {
                    TransportEntry::TransportBuilder(TransportBuilder::new(
                        &mut self.transport_state,
                        TransportRequiredChanges::new(id, &mut self.transport_changes),
                        transport_type,
                        self.options.rtcp_mux_policy,
                        self.options.offer_ice,
                    ))
                });

                (
                    Some(standalone_transport_id),
                    bundle_transport_id.unwrap_or(standalone_transport_id),
                )
            }
            BundlePolicy::MaxBundle => {
                // Force bundling, only create a transport if none exists yet
                let transport_id = if let Some(existing_transport) = bundle_transport_id {
                    existing_transport
                } else {
                    self.transports.insert_with_key(|id| {
                        TransportEntry::TransportBuilder(TransportBuilder::new(
                            &mut self.transport_state,
                            TransportRequiredChanges::new(id, &mut self.transport_changes),
                            transport_type,
                            self.options.rtcp_mux_policy,
                            self.options.offer_ice,
                        ))
                    })
                };

                (None, transport_id)
            }
        };

        self.pending_changes
            .push(PendingChange::AddMedia(PendingMedia {
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

    /// Receive a SDP offer in this session.
    ///
    /// Returns an opaque response state object which can be used to create the actual response SDP.
    /// Before the SDP response can be created, the user must make all necessary changes to the transports using [`transport_changes`](Self::transport_changes)
    ///
    /// The actual answer can be created using [`create_sdp_answer`](Self::create_sdp_answer).
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
            let transport = self.get_or_create_transport(&new_state, &offer, remote_media_desc)?;

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
                next_rtcp: Instant::now() + Duration::from_secs(5),
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

        self.remove_unused_transports();

        Ok(SdpAnswerState(response))
    }

    /// Remove all transports that are not being used anymore
    fn remove_unused_transports(&mut self) {
        self.transports.retain(|id, _| {
            // Is the transport in use by active media?
            let in_use_by_active = self.state.iter().any(|media| media.transport == id);

            // Is the transport in use by any pending changes?
            let in_use_by_pending = self.pending_changes.iter().any(|change| {
                if let PendingChange::AddMedia(add_media) = change {
                    add_media.bundle_transport == id || add_media.standalone_transport == Some(id)
                } else {
                    false
                }
            });

            if in_use_by_active || in_use_by_pending {
                true
            } else {
                self.transport_changes.push(TransportChange::Remove(id));
                false
            }
        });
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
        session_desc: &SessionDescription,
        remote_media_desc: &MediaDescription,
    ) -> Result<Option<TransportId>, Error> {
        // See if there's a transport to be reused via BUNDLE group
        if let Some(id) = remote_media_desc
            .mid
            .as_ref()
            .and_then(|mid| self.find_bundled_transport(new_state, session_desc, mid))
        {
            return Ok(Some(id));
        }

        // TODO: this is very messy, create_from_offer return Ok(None) if the transport is not supported
        let maybe_transport_id =
            self.transports
                .try_insert_with_key(|id| -> Result<TransportEntry, Option<_>> {
                    Transport::create_from_offer(
                        Self::propagate_transport_events(&self.state, &mut self.events, id),
                        &mut self.transport_state,
                        TransportRequiredChanges::new(id, &mut self.transport_changes),
                        session_desc,
                        remote_media_desc,
                    )
                    .map_err(Some)?
                    .map(TransportEntry::Transport)
                    .ok_or(None)
                });

        match maybe_transport_id {
            Ok(id) => Ok(Some(id)),
            Err(Some(err)) => Err(err),
            Err(None) => Ok(None),
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

            media_descriptions.push(self.media_description_for_active(active, None));
        }

        let mut sess_desc = SessionDescription {
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
            group: self.build_bundle_groups(false),
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
        };

        if let Some(ice_credentials) = self
            .transports
            .values()
            .find_map(|t| Some(t.ice_agent()?.credentials()))
        {
            sess_desc.ice_ufrag = Some(IceUsernameFragment {
                ufrag: ice_credentials.ufrag.clone().into(),
            });

            sess_desc.ice_pwd = Some(IcePassword {
                pwd: ice_credentials.pwd.clone().into(),
            });
        }

        sess_desc
    }

    pub fn create_sdp_offer(&self) -> SessionDescription {
        let mut media_descriptions = vec![];

        // Put the current media sessions in the offer
        for media in &self.state {
            let mut override_direction = None;

            // Apply requested changes
            for change in &self.pending_changes {
                match change {
                    PendingChange::AddMedia(..) => {}
                    PendingChange::RemoveMedia(media_id) => {
                        if media.id == *media_id {
                            continue;
                        }
                    }
                    PendingChange::ChangeDirection(media_id, direction) => {
                        if media.id == *media_id {
                            override_direction = Some(*direction);
                        }
                    }
                }
            }

            media_descriptions.push(self.media_description_for_active(media, override_direction));
        }

        // Add all pending added media
        for change in &self.pending_changes {
            let PendingChange::AddMedia(pending_media) = change else {
                continue;
            };

            let local_media = &self.local_media[pending_media.local_media_id];
            let transport = &self.transports[pending_media
                .standalone_transport
                .unwrap_or(pending_media.bundle_transport)];

            let (local_rtp_port, local_rtcp_port) = transport.ports();

            let mut rtpmap = vec![];
            let mut fmtp = vec![];
            let mut fmts = vec![];

            for codec in &local_media.codecs.codecs {
                let pt = codec.pt.expect("pt is set when adding the codec");

                fmts.push(pt);

                rtpmap.push(RtpMap {
                    payload: pt,
                    encoding: codec.name.as_ref().into(),
                    clock_rate: codec.clock_rate,
                    params: codec.channels.map(|c| c.to_string().into()),
                });

                // TODO: are multiple fmtps allowed?
                for param in &codec.params {
                    fmtp.push(Fmtp {
                        format: pt,
                        params: param.clone().into(),
                    });
                }
            }

            let mut media_desc = MediaDescription {
                media: Media {
                    media_type: local_media.codecs.media_type,
                    port: local_rtp_port.expect("rtp port not set for transport"),
                    ports_num: None,
                    proto: transport.type_().sdp_type(),
                    fmts,
                },
                connection: None,
                bandwidth: vec![],
                direction: pending_media.direction.into(),
                rtcp: local_rtcp_port.map(|port| Rtcp {
                    port,
                    address: None,
                }),
                // always offer rtcp-mux
                rtcp_mux: true,
                mid: Some(pending_media.mid.as_str().into()),
                rtpmap,
                fmtp,
                ice_ufrag: None,
                ice_pwd: None,
                ice_candidates: vec![],
                ice_end_of_candidates: false,
                crypto: vec![],
                extmap: vec![],
                extmap_allow_mixed: false,
                ssrc: vec![],
                setup: None,
                fingerprint: vec![],
                attributes: vec![],
            };

            transport.populate_desc(&mut media_desc);

            media_descriptions.push(media_desc);
        }

        let mut sess_desc = SessionDescription {
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
            group: self.build_bundle_groups(true),
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
        };

        if let Some(ice_credentials) = self
            .transports
            .values()
            .find_map(|t| Some(t.ice_agent()?.credentials()))
        {
            sess_desc.ice_ufrag = Some(IceUsernameFragment {
                ufrag: ice_credentials.ufrag.clone().into(),
            });

            sess_desc.ice_pwd = Some(IcePassword {
                pwd: ice_credentials.pwd.clone().into(),
            });
        }

        sess_desc
    }

    /// Receive a SDP answer after sending an offer.
    pub fn receive_sdp_answer(&mut self, answer: SessionDescription) {
        'next_media_desc: for remote_media_desc in &answer.media_descriptions {
            // Skip any rejected answers
            if remote_media_desc.direction == Direction::Inactive {
                continue;
            }

            let requested_direction: DirectionBools = remote_media_desc.direction.flipped().into();

            // Try to match an active media session, while filtering out media that is to be deleted
            for media in &mut self.state {
                let pending_removal = self
                    .pending_changes
                    .iter()
                    .filter_map(PendingChange::remove_media)
                    .any(|removed| removed == media.id);

                if pending_removal {
                    // Ignore this active media since it's supposed to be removed
                    continue;
                }

                if media.matches(&self.transports, remote_media_desc) {
                    // TODO: update media
                    let _ = requested_direction;
                    continue 'next_media_desc;
                }
            }

            // Try to match a new media session
            for pending_change in &self.pending_changes {
                let PendingChange::AddMedia(pending_media) = pending_change else {
                    continue;
                };

                if !pending_media.matches_answer(&self.transports, remote_media_desc) {
                    continue;
                }

                // Check which transport to use, (standalone or bundled)
                let is_bundled = answer.group.iter().any(|group| {
                    group.typ == "BUNDLE"
                        && group.mids.iter().any(|m| m.as_str() == pending_media.mid)
                });

                let transport_id = if is_bundled {
                    pending_media.bundle_transport
                } else {
                    // TODO: return an error here instead, we required BUNDLE, but it is not supported
                    pending_media.standalone_transport.unwrap()
                };

                // Build transport if necessary
                if let TransportEntry::TransportBuilder(transport_builder) =
                    &mut self.transports[transport_id]
                {
                    let transport_builder =
                        replace(transport_builder, TransportBuilder::placeholder());

                    let transport = transport_builder.build_from_answer(
                        Self::propagate_transport_events(
                            &self.state,
                            &mut self.events,
                            transport_id,
                        ),
                        &mut self.transport_state,
                        TransportRequiredChanges::new(transport_id, &mut self.transport_changes),
                        &answer,
                        remote_media_desc,
                    );

                    self.transports[transport_id] = TransportEntry::Transport(transport);
                }

                let (codec, codec_pt, direction) = self.local_media[pending_media.local_media_id]
                    .choose_codec_from_answer(remote_media_desc)
                    .unwrap();

                self.state.push(ActiveMedia {
                    id: pending_media.id,
                    local_media_id: pending_media.local_media_id,
                    media_type: pending_media.media_type,
                    rtp_session: RtpSession::new(rand::random(), codec.clock_rate),
                    next_rtcp: Instant::now() + Duration::from_secs(5),
                    mid: remote_media_desc.mid.clone(),
                    direction,
                    transport: transport_id,
                    codec_pt,
                    codec,
                });
            }
        }

        self.pending_changes.clear();
        self.remove_unused_transports();
    }

    fn media_description_for_active(
        &self,
        active: &ActiveMedia,
        override_direction: Option<Direction>,
    ) -> MediaDescription {
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
                proto: transport.type_().sdp_type(),
                fmts: vec![active.codec_pt],
            },
            connection: None,
            bandwidth: vec![],
            direction: override_direction.unwrap_or(active.direction.into()),
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
            extmap: vec![],
            extmap_allow_mixed: false,
            ssrc: vec![],
            setup: None,
            fingerprint: vec![],
            attributes: vec![],
        };

        transport.populate_desc(&mut media_desc);

        media_desc
    }

    fn build_bundle_groups(&self, include_pending_changes: bool) -> Vec<Group> {
        let mut bundle_groups: HashMap<TransportId, Vec<BytesStr>> = HashMap::new();

        for media in &self.state {
            if let Some(mid) = media.mid.clone() {
                bundle_groups.entry(media.transport).or_default().push(mid);
            }
        }

        if include_pending_changes {
            for change in &self.pending_changes {
                if let PendingChange::AddMedia(pending_media) = change {
                    bundle_groups
                        .entry(pending_media.bundle_transport)
                        .or_default()
                        .push(pending_media.mid.as_str().into());
                }
            }
        }

        bundle_groups
            .into_values()
            .filter(|c| !c.is_empty())
            .map(|mids| Group {
                typ: BytesStr::from_static("BUNDLE"),
                mids,
            })
            .collect()
    }

    /// Returns an iterator which contains all pending transport changes
    pub fn transport_changes(&mut self) -> Vec<TransportChange> {
        std::mem::take(&mut self.transport_changes)
    }

    /// Set the RTP/RTCP ports of a transport
    pub fn set_transport_ports(
        &mut self,
        transport_id: TransportId,
        ip_addrs: &[IpAddr],
        rtp_port: u16,
        rtcp_port: Option<u16>,
    ) {
        let transport = &mut self.transports[transport_id];

        match transport {
            TransportEntry::Transport(transport) => {
                transport.local_rtp_port = Some(rtp_port);
                transport.local_rtcp_port = rtcp_port;
            }
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.local_rtp_port = Some(rtp_port);
                transport_builder.local_rtcp_port = rtcp_port;
            }
        };

        if let Some(ice_agent) = transport.ice_agent_mut() {
            for ip in ip_addrs {
                ice_agent.add_host_addr(SocketUse::Rtp, SocketAddr::new(*ip, rtp_port));

                if let Some(rtcp_port) = rtcp_port {
                    ice_agent.add_host_addr(SocketUse::Rtcp, SocketAddr::new(*ip, rtcp_port));
                }
            }
        }
    }

    /// Returns a duration after which [`poll`](Self::poll) must be called
    pub fn timeout(&self) -> Option<Duration> {
        let now = Instant::now();

        let mut timeout = None;

        for transport in self.transports.values() {
            match transport {
                TransportEntry::Transport(transport) => {
                    timeout = opt_min(timeout, transport.timeout(now))
                }
                TransportEntry::TransportBuilder(transport_builder) => {
                    timeout = opt_min(timeout, transport_builder.timeout(now))
                }
            }
        }

        for media in self.state.iter() {
            timeout = opt_min(timeout, media.rtp_session.pop_rtp_after(None));

            let rtcp_send_timeout = media
                .next_rtcp
                .checked_duration_since(now)
                .unwrap_or_default();
            timeout = opt_min(timeout, Some(rtcp_send_timeout))
        }

        timeout
    }

    /// Poll for new events. Call [`pop_event`](Self::pop_event) to handle them.
    pub fn poll(&mut self) {
        for (transport_id, transport) in &mut self.transports {
            match transport {
                TransportEntry::Transport(transport) => {
                    transport.poll(Self::propagate_transport_events(
                        &self.state,
                        &mut self.events,
                        transport_id,
                    ));
                }
                TransportEntry::TransportBuilder(transport_builder) => {
                    transport_builder.poll(Self::propagate_transport_events(
                        &self.state,
                        &mut self.events,
                        transport_id,
                    ));
                }
            }
        }

        for media in self.state.iter_mut() {
            if let Some(rtp_packet) = media.rtp_session.pop_rtp(None) {
                self.events.push(Event::ReceiveRTP {
                    media_id: media.id,
                    packet: rtp_packet,
                });
            }

            // if media.next_rtcp <= now {
            //     send_rtcp_report(
            //         &mut self.events,
            //         self.transports[media.transport].unwrap_mut(),
            //         media,
            //     );

            //     media.next_rtcp += Duration::from_secs(5);
            // }
        }
    }

    /// "Converts" the the transport's event to the session events
    fn propagate_transport_events<'a>(
        state: &'a [ActiveMedia],
        events: &'a mut Events,
        transport_id: TransportId,
    ) -> impl FnMut(TransportEvent) + use<'a> {
        move |event| {
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
                    source,
                    target,
                } => {
                    events.push(Event::SendData {
                        socket: SocketId(transport_id, socket),
                        data,
                        source,
                        target,
                    });
                }
            }
        }
    }

    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    pub fn receive(&mut self, transport_id: TransportId, mut pkt: ReceivedPkt) {
        let transport = match &mut self.transports[transport_id] {
            TransportEntry::Transport(transport) => transport,
            TransportEntry::TransportBuilder(transport_builder) => {
                transport_builder.receive(pkt);
                return;
            }
        };

        match transport.receive(
            Self::propagate_transport_events(&self.state, &mut self.events, transport_id),
            &mut pkt,
        ) {
            ReceivedPacket::Rtp => {
                let rtp_packet = match RtpPacket::parse(&pkt.data) {
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
                    .filter(|m| m.transport == transport_id)
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
                        .filter(|m| m.transport == transport_id)
                        .find(|e| e.codec_pt == packet.payload_type())
                };

                if let Some(entry) = entry {
                    entry.rtp_session.recv_rtp(rtp_packet);
                } else {
                    log::warn!("Failed to find media for RTP packet ssrc={}", packet.ssrc());
                }
            }
            ReceivedPacket::Rtcp => {
                let rtcp_compound = match Compound::parse(&pkt.data) {
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
            source: None, // TODO: set this according to the transport
            target: transport.remote_rtp_address,
        });
    }
}

fn send_rtcp_report(events: &mut Events, transport: &mut Transport, media: &mut ActiveMedia) {
    let mut encode_buf = vec![0u8; 65535];

    let len = match media.rtp_session.write_rtcp_report(&mut encode_buf) {
        Ok(len) => len,
        Err(e) => {
            log::warn!("Failed to write RTCP packet, {e:?}");
            return;
        }
    };

    encode_buf.truncate(len);
    transport.protect_rtcp(&mut encode_buf);

    let socket_use = if transport.rtcp_mux {
        SocketUse::Rtp
    } else {
        SocketUse::Rtcp
    };

    events.push(Event::SendData {
        socket: SocketId(media.transport, socket_use),
        data: encode_buf,
        source: None, // TODO: set this according to the transport
        target: transport.remote_rtcp_address,
    });
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

fn opt_min<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (None, Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        (Some(a), Some(b)) => Some(min(a, b)),
    }
}
