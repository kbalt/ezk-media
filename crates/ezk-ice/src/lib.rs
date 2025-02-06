#![deny(unreachable_pub, unsafe_code)]

//! sans io implementation of an ICE agent

use core::fmt;
use rand::distributions::{Alphanumeric, DistString};
use sdp_types::{IceCandidate, UntaggedAddress};
use slotmap::{new_key_type, SlotMap};
use std::{
    cmp::{max, min},
    collections::VecDeque,
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use stun::{StunConfig, StunServerBinding};
use stun_types::{
    attributes::{
        ErrorCode, Fingerprint, IceControlled, IceControlling, Priority, UseCandidate,
        XorMappedAddress,
    },
    Class, Message, TransactionId,
};

mod stun;

/// A message received on a UDP socket
pub struct ReceivedPkt {
    /// The received data
    pub data: Vec<u8>,
    /// Source address of the message
    pub source: SocketAddr,
    /// Local socket destination address of the message
    pub destination: SocketAddr,
    /// On which component socket this was received
    pub component: Component,
}

/// Component of the data stream
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Component {
    /// The RTP component of the data stream. This will also contain RTCP if rtcp-mux is enabled.
    Rtp = 1,
    /// The RTCP component of the data stream. This will not be used if rtcp-mux is enabled.
    Rtcp = 2,
}

/// ICE related events emitted by the [`IceAgent`]
#[derive(Debug)]
pub enum IceEvent {
    GatheringStateChanged {
        old: IceGatheringState,
        new: IceGatheringState,
    },
    ConnectionStateChanged {
        old: IceConnectionState,
        new: IceConnectionState,
    },
    UseAddr {
        component: Component,
        target: SocketAddr,
    },
    SendData {
        component: Component,
        data: Vec<u8>,
        source: Option<IpAddr>,
        target: SocketAddr,
    },
}

/// The ICE agent state machine
pub struct IceAgent {
    stun_config: StunConfig,

    local_credentials: IceCredentials,
    remote_credentials: Option<IceCredentials>,

    local_candidates: SlotMap<LocalCandidateId, Candidate>,
    remote_candidates: SlotMap<RemoteCandidateId, Candidate>,

    stun_server: Vec<StunServerBinding>,

    rtcp_mux: bool,
    is_controlling: bool,
    control_tie_breaker: u64,
    max_pairs: usize,

    gathering_state: IceGatheringState,
    connection_state: IceConnectionState,

    pairs: Vec<CandidatePair>,
    triggered_check_queue: VecDeque<(LocalCandidateId, RemoteCandidateId)>,

    last_ta_trigger: Option<Instant>,
}

/// State of gathering candidates from external (STUN/TURN) servers.
/// If no STUN server is configured this state will jump directly to `Complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceGatheringState {
    /// The ICE agent was just created
    New,
    /// The ICE agent is in the process of gathering candidates
    Gathering,
    /// The ICE agent has finished gathering candidates. If something happens that requires collecting new candidates,
    /// such as the addition of a new ICE server, the state will revert to `Gathering` to gather those candidates.
    Complete,
}

/// State of the ICE agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceConnectionState {
    /// The ICE agent is awaiting local & remote ice candidates
    New,
    /// The ICE agent is in the process of checking candidates pairs
    Checking,
    /// The ICE agent has found a valid pair for all components
    Connected,

    // TODO: this state is currently unreachable since the first valid pair is instantly nominated
    //Completed,
    //
    /// The ICE agent has failed to find a valid candidate pair for all components
    Failed,
    /// Checks to ensure that components are still connected failed for at least one component.
    /// This is a less stringent test than failed and may trigger intermittently and resolve just as spontaneously on
    /// less reliable networks, or during temporary disconnections.
    /// When the problem resolves, the connection may return to the connected state.
    Disconnected,
}

new_key_type!(
    struct LocalCandidateId;
    struct RemoteCandidateId;
);

#[derive(Debug, PartialEq, Clone, Copy, Hash)]
enum CandidateKind {
    Host = 126,
    PeerReflexive = 110,
    ServerReflexive = 100,
    // TODO: Relayed = 0,
}

struct Candidate {
    addr: SocketAddr,
    // transport: udp
    kind: CandidateKind,
    priority: u32,
    foundation: String,

    component: Component,

    // The transport address that an ICE agent sends from for a particular candidate.
    // For host, server-reflexive, and peer-reflexive candidates, the base is the same as the host candidate.
    // For relayed candidates, the base is the same as the relayed candidate
    //  (i.e., the transport address used by the TURN server to send from).
    base: SocketAddr,
}

struct CandidatePair {
    local: LocalCandidateId,
    remote: RemoteCandidateId,
    priority: u64,
    state: CandidatePairState,
    component: Component,

    // Nominated by the peer
    received_use_candidate: bool,
    // Nominated by us
    nominated: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum CandidatePairState {
    /// A check has not been sent for this pair, but the pair is not Frozen.
    Waiting,

    /// A check has been sent for this pair, but the transaction is in progress.
    InProgress {
        transaction_id: TransactionId,
        stun_request: Vec<u8>,
        retransmit_at: Instant,
        retransmits: u32,
        source: IpAddr,
        target: SocketAddr,
    },

    // A check has been sent for this pair, and it produced a successful result.
    Succeeded,

    /// A check has been sent for this pair, and it failed (a response to the check
    /// was never received, or a failure response was received).
    Failed,
}

/// Credentials of an ICE agent
///
/// These must be exchanges using some external signaling protocol like SDP
#[derive(Clone)]
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
}

impl IceCredentials {
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();

        Self {
            ufrag: Alphanumeric.sample_string(&mut rng, 8),
            pwd: Alphanumeric.sample_string(&mut rng, 32),
        }
    }
}

impl IceAgent {
    pub fn new_from_answer(
        local_credentials: IceCredentials,
        remote_credentials: IceCredentials,
        is_controlling: bool,
        rtcp_mux: bool,
    ) -> Self {
        IceAgent {
            stun_config: StunConfig::new(),
            local_credentials,
            remote_credentials: Some(remote_credentials),
            local_candidates: SlotMap::with_key(),
            remote_candidates: SlotMap::with_key(),
            stun_server: vec![],
            rtcp_mux,
            is_controlling,
            control_tie_breaker: rand::random(),
            max_pairs: 100,
            gathering_state: IceGatheringState::New,
            connection_state: IceConnectionState::New,
            pairs: Vec::new(),
            triggered_check_queue: VecDeque::new(),
            last_ta_trigger: None,
        }
    }

    pub fn new_for_offer(
        local_credentials: IceCredentials,
        is_controlling: bool,
        rtcp_mux: bool,
    ) -> Self {
        IceAgent {
            stun_config: StunConfig::new(),
            local_credentials,
            remote_credentials: None,
            local_candidates: SlotMap::with_key(),
            remote_candidates: SlotMap::with_key(),
            stun_server: vec![],
            rtcp_mux,
            is_controlling,
            control_tie_breaker: rand::random(),
            max_pairs: 100,
            gathering_state: IceGatheringState::New,
            connection_state: IceConnectionState::New,
            pairs: Vec::new(),
            triggered_check_queue: VecDeque::new(),
            last_ta_trigger: None,
        }
    }

    /// Set all the remote information in one step. This function is usually set once after receiving a SDP answer.
    pub fn set_remote_data(
        &mut self,
        credentials: IceCredentials,
        candidates: &[IceCandidate],
        rtcp_mux: bool,
    ) {
        // TODO: assert that we can't change from rtcp-mux: true -> false
        self.rtcp_mux = rtcp_mux;

        // Remove all rtcp candidates and stun server bindings rtcp-mux is enabled
        if rtcp_mux {
            self.stun_server.retain(|s| s.component() == Component::Rtp);
            self.local_candidates
                .retain(|_, c| c.component == Component::Rtp);
        }

        self.remote_credentials = Some(credentials);

        for candidate in candidates {
            self.add_remote_candidate(candidate);
        }
    }

    /// Return the ice-agent's ice credentials
    pub fn credentials(&self) -> &IceCredentials {
        &self.local_credentials
    }

    /// Register a host address for a given ICE component. This will be used to create a host candidate.
    /// For the ICE agent to work properly, all available ip addresses of the host system should be provided.
    pub fn add_host_addr(&mut self, component: Component, addr: SocketAddr) {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            return;
        }

        if let SocketAddr::V6(v6) = addr {
            let ip = v6.ip();
            if ip.to_ipv4().is_some() || ip.to_ipv4_mapped().is_some() {
                return;
            }
        }

        self.add_local_candidate(component, CandidateKind::Host, addr, addr);
    }

    /// Add a STUN server which the ICE agent should use to gather additional (server-reflexive) candidates.
    pub fn add_stun_server(&mut self, server: SocketAddr) {
        self.stun_server
            .push(StunServerBinding::new(server, Component::Rtp));

        if !self.rtcp_mux {
            self.stun_server
                .push(StunServerBinding::new(server, Component::Rtcp));
        }
    }

    /// Returns the current ICE candidate gathering state
    pub fn gathering_state(&self) -> IceGatheringState {
        self.gathering_state
    }

    /// Returns the current ICE connection state
    pub fn connection_state(&self) -> IceConnectionState {
        self.connection_state
    }

    fn add_local_candidate(
        &mut self,
        component: Component,
        kind: CandidateKind,
        base: SocketAddr,
        addr: SocketAddr,
    ) {
        // Check if we need to create a new candidate for this
        let already_exists = self
            .local_candidates
            .values()
            .any(|c| c.kind == kind && c.base == base && c.addr == addr);

        if already_exists {
            // ignore
            return;
        }

        log::debug!("add local candidate {component:?} {kind:?} {addr}");

        // Calculate the candidate priority using offsets + count of candidates of the same type
        // (trick that I have stolen from str0m's implementation)
        let local_preference_offset = match kind {
            CandidateKind::Host => (65535 / 4) * 3,
            CandidateKind::PeerReflexive => (65535 / 4) * 2,
            CandidateKind::ServerReflexive => 65535 / 4,
            // CandidateKind::Relayed => 0,
        };

        let local_preference = self
            .local_candidates
            .values()
            .filter(|c| c.kind == kind)
            .count() as u32
            + local_preference_offset;

        let kind_preference = (kind as u32) << 24;
        let local_preference = local_preference << 8;
        let priority = kind_preference + local_preference + (256 - component as u32);

        self.local_candidates.insert(Candidate {
            addr,
            kind,
            priority,
            foundation: compute_foundation(kind, base.ip(), None, "udp").to_string(),
            component,
            base,
        });

        self.form_pairs();
    }

    /// Add a peer's ice-candidate which has been received using an extern signaling protocol
    pub fn add_remote_candidate(&mut self, candidate: &IceCandidate) {
        let kind = match candidate.typ.as_str() {
            "host" => CandidateKind::Host,
            "srflx" => CandidateKind::ServerReflexive,
            _ => return,
        };

        // TODO: currently only udp transport is supported
        if !candidate.transport.eq_ignore_ascii_case("udp") {
            return;
        }

        let component = match candidate.component {
            1 => Component::Rtp,
            // Discard candidates for rtcp if rtcp-mux is enabled
            2 if !self.rtcp_mux => Component::Rtcp,
            _ => {
                log::debug!(
                    "Discard remote candidate with unsupported component candidate:{candidate}"
                );
                return;
            }
        };

        let ip = match candidate.address {
            UntaggedAddress::Fqdn(..) => return,
            UntaggedAddress::IpAddress(ip_addr) => ip_addr,
        };

        self.remote_candidates.insert(Candidate {
            addr: SocketAddr::new(ip, candidate.port),
            kind,
            priority: u32::try_from(candidate.priority).unwrap(),
            foundation: candidate.foundation.to_string(),
            component,
            base: SocketAddr::new(ip, candidate.port), // TODO: do I even need this?
        });

        self.form_pairs();
    }

    fn form_pairs(&mut self) {
        for (local_id, local_candidate) in &self.local_candidates {
            for (remote_id, remote_candidate) in &self.remote_candidates {
                // Remote peer-reflexive candidates are not paired here
                if remote_candidate.kind == CandidateKind::PeerReflexive {
                    continue;
                }

                // Do not pair candidates with different components
                if local_candidate.component != remote_candidate.component {
                    continue;
                }

                // Check if the pair already exists
                let already_exists = self
                    .pairs
                    .iter()
                    .any(|pair| pair.local == local_id && pair.remote == remote_id);

                if already_exists {
                    continue;
                }

                // Exclude pairs with different ip version
                match (local_candidate.addr.ip(), remote_candidate.addr.ip()) {
                    (IpAddr::V4(l), IpAddr::V4(r)) if l.is_link_local() == r.is_link_local() => {
                        /* ok */
                    }
                    // Only pair IPv6 addresses when either both or neither are link local addresses
                    (IpAddr::V6(l), IpAddr::V6(r))
                        if l.is_unicast_link_local() == r.is_unicast_link_local() =>
                    { /* ok */ }
                    _ => {
                        // Would make an invalid pair, skip
                        continue;
                    }
                }

                Self::add_candidate_pair(
                    local_id,
                    local_candidate,
                    remote_id,
                    remote_candidate,
                    self.is_controlling,
                    &mut self.pairs,
                    false,
                );
            }
        }

        self.pairs.sort_unstable_by_key(|p| p.priority);

        self.prune_pairs();
    }

    fn add_candidate_pair(
        local_id: LocalCandidateId,
        local_candidate: &Candidate,
        remote_id: RemoteCandidateId,
        remote_candidate: &Candidate,
        is_controlling: bool,
        pairs: &mut Vec<CandidatePair>,
        received_use_candidate: bool,
    ) {
        if pairs
            .iter()
            .any(|p| p.local == local_id && p.remote == remote_id)
        {
            // pair already exists
            return;
        }

        let priority = pair_priority(local_candidate, remote_candidate, is_controlling);

        log::debug!(
            "add pair {}, priority: {priority}, component={:?}",
            DisplayPair(local_candidate, remote_candidate),
            local_candidate.component,
        );

        pairs.push(CandidatePair {
            local: local_id,
            remote: remote_id,
            priority,
            state: CandidatePairState::Waiting,
            component: local_candidate.component,
            received_use_candidate,
            nominated: false,
        });
        pairs.sort_unstable_by_key(|p| p.priority);
    }

    fn recompute_pair_priorities(&mut self) {
        for pair in &mut self.pairs {
            pair.priority = pair_priority(
                &self.local_candidates[pair.local],
                &self.remote_candidates[pair.remote],
                self.is_controlling,
            );
        }

        self.pairs.sort_unstable_by_key(|p| p.priority);
    }

    /// Prune the lowest priority pairs until `max_pairs` is reached
    fn prune_pairs(&mut self) {
        while self.pairs.len() > self.max_pairs {
            let pair = self.pairs.pop().unwrap();
            log::debug!("Pruned pair {:?}:{:?}", pair.local, pair.remote);
        }
    }

    /// Receive network packets for this ICE agent
    pub fn receive(&mut self, on_event: impl FnMut(IceEvent), pkt: &ReceivedPkt) {
        // TODO: avoid clone here, this should be free
        let mut stun_msg = Message::parse(pkt.data.clone()).unwrap();

        let passed_fingerprint_check = stun_msg
            .attribute::<Fingerprint>()
            .is_some_and(|r| r.is_ok());

        if !passed_fingerprint_check {
            log::trace!(
                "Incoming STUN {:?} failed fingerprint check, discarding",
                stun_msg.class()
            );
            return;
        }

        match stun_msg.class() {
            Class::Request => self.receive_stun_request(on_event, pkt, stun_msg),
            Class::Indication => { /* ignore */ }
            Class::Success => self.receive_stun_success(on_event, pkt, stun_msg),
            Class::Error => self.receive_stun_error(stun_msg),
        }
    }

    fn receive_stun_success(
        &mut self,
        mut on_event: impl FnMut(IceEvent),
        pkt: &ReceivedPkt,
        mut stun_msg: Message,
    ) {
        // Check our stun server binding checks before verifying integrity since these aren't authenticated
        for stun_server_binding in &mut self.stun_server {
            if !stun_server_binding.wants_stun_response(stun_msg.transaction_id()) {
                continue;
            }

            let Some(addr) =
                stun_server_binding.receive_stun_response(&self.stun_config, pkt, stun_msg)
            else {
                // TODO; no xor mapped in response, discard message
                return;
            };

            let component = stun_server_binding.component();
            self.add_local_candidate(
                component,
                CandidateKind::ServerReflexive,
                pkt.destination,
                addr,
            );

            return;
        }

        if !stun::verify_integrity(
            &self.local_credentials,
            &self.remote_credentials,
            &mut stun_msg,
        ) {
            log::debug!("Incoming stun success failed the integrity check, discarding");
            return;
        }

        // A connectivity check is considered a success if each of the following
        // criteria is true:
        // o  The Binding request generated a success response; and
        // o  The source and destination transport addresses in the Binding
        //    request and response are symmetric.
        let Some(pair) = self
            .pairs
            .iter_mut()
            .find(|p| {
                matches!(p.state, CandidatePairState::InProgress { transaction_id, .. } if stun_msg.transaction_id() == transaction_id)
            }) else {
                log::debug!("Failed to find transaction for STUN success, discarding");
                return;
            };

        let CandidatePairState::InProgress { source, target, .. } = &pair.state else {
            unreachable!()
        };

        if pkt.source == *target || pkt.destination.ip() == *source {
            log::debug!(
                "got success response for pair {} nominated={}",
                DisplayPair(
                    &self.local_candidates[pair.local],
                    &self.remote_candidates[pair.remote],
                ),
                pair.nominated,
            );

            // This request was a nomination for this pair
            if pair.nominated {
                let local_candidate = &self.local_candidates[pair.local];
                let remote_candidate = &self.remote_candidates[pair.remote];

                on_event(IceEvent::UseAddr {
                    component: local_candidate.component,
                    target: remote_candidate.addr,
                });
            }

            pair.state = CandidatePairState::Succeeded;
        } else {
            log::debug!(
                "got success response with invalid source address for pair {}",
                DisplayPair(
                    &self.local_candidates[pair.local],
                    &self.remote_candidates[pair.remote]
                )
            );

            // The ICE agent MUST check that the source and destination transport addresses in the Binding request and
            // response are symmetric. That is, the source IP address and port of the response MUST be equal to the
            // destination IP address and port to which the Binding request was sent, and the destination IP address and
            // port of the response MUST be equal to the source IP address and port from which the Binding request was sent.
            // If the addresses are not symmetric, the agent MUST set the candidate pair state to Failed.
            pair.nominated = false;
            pair.state = CandidatePairState::Failed;
        }

        // Check if we discover a new peer-reflexive candidate here
        let mapped_addr = stun_msg.attribute::<XorMappedAddress>().unwrap().unwrap();
        if mapped_addr.0 != self.local_candidates[pair.local].addr {
            let component = pair.component;
            self.add_local_candidate(
                component,
                CandidateKind::PeerReflexive,
                pkt.destination,
                mapped_addr.0,
            );
        }
    }

    fn receive_stun_error(&mut self, mut stun_msg: Message) {
        if !stun::verify_integrity(
            &self.local_credentials,
            &self.remote_credentials,
            &mut stun_msg,
        ) {
            log::debug!("Incoming stun error response failed the integrity check, discarding");
            return;
        }

        let Some(pair) = self
            .pairs
            .iter_mut()
            .find(|p| {
                matches!(p.state, CandidatePairState::InProgress { transaction_id, .. } if stun_msg.transaction_id() == transaction_id)
            }) else {
                log::debug!("Failed to find transaction for STUN error, discarding");
                return;
            };

        if let Some(Ok(error_code)) = stun_msg.attribute::<ErrorCode>() {
            log::debug!(
                "Candidate pair failed with code={}, reason={}",
                error_code.number,
                error_code.reason
            );

            // If the Binding request generates a 487 (Role Conflict) error response,
            // and if the ICE agent included an ICE-CONTROLLED attribute in the request,
            // the agent MUST switch to the controlling role.
            // If the agent included an ICE-CONTROLLING attribute in the request, the agent MUST switch to the controlled role.
            if error_code.number == 487 {
                if stun_msg.attribute::<IceControlled>().is_some() {
                    self.is_controlling = true;
                } else if stun_msg.attribute::<IceControlling>().is_some() {
                    self.is_controlling = false;
                }

                // Once the agent has switched its role, the agent MUST add the
                // candidate pair whose check generated the 487 error response to the
                // triggered-check queue associated with the checklist to which the pair
                // belongs, and set the candidate pair state to Waiting.
                pair.state = CandidatePairState::Waiting;
                self.triggered_check_queue
                    .push_back((pair.local, pair.remote));

                // A role switch requires an agent to recompute pair priorities, since the priority values depend on the role.
                self.recompute_pair_priorities();
            }
        }
    }

    fn receive_stun_request(
        &mut self,
        mut on_event: impl FnMut(IceEvent),
        pkt: &ReceivedPkt,
        mut stun_msg: Message,
    ) {
        if !stun::verify_integrity(
            &self.local_credentials,
            &self.remote_credentials,
            &mut stun_msg,
        ) {
            log::debug!("Incoming stun request failed the integrity check, discarding");
            return;
        }

        let priority = stun_msg.attribute::<Priority>().unwrap().unwrap();
        let use_candidate = stun_msg.attribute::<UseCandidate>().is_some();

        // Detect and handle role conflict
        if self.is_controlling {
            if let Some(Ok(ice_controlling)) = stun_msg.attribute::<IceControlling>() {
                if self.control_tie_breaker >= ice_controlling.0 {
                    let response = stun::make_role_error(
                        stun_msg.transaction_id(),
                        &self.local_credentials,
                        self.remote_credentials.as_ref().unwrap(),
                        pkt.source,
                        true,
                        self.control_tie_breaker,
                    );

                    on_event(IceEvent::SendData {
                        component: pkt.component,
                        data: response,
                        source: Some(pkt.destination.ip()),
                        target: pkt.source,
                    });

                    return;
                } else {
                    self.is_controlling = false;
                    self.recompute_pair_priorities();
                }
            }
        } else if !self.is_controlling {
            if let Some(Ok(ice_controlled)) = stun_msg.attribute::<IceControlled>() {
                if self.control_tie_breaker >= ice_controlled.0 {
                    let response = stun::make_role_error(
                        stun_msg.transaction_id(),
                        &self.local_credentials,
                        self.remote_credentials.as_ref().unwrap(),
                        pkt.source,
                        false,
                        self.control_tie_breaker,
                    );

                    on_event(IceEvent::SendData {
                        component: pkt.component,
                        data: response,
                        source: Some(pkt.destination.ip()),
                        target: pkt.source,
                    });
                    return;
                } else {
                    self.is_controlling = true;
                    self.recompute_pair_priorities();
                }
            }
        }

        let local_id = match self
            .local_candidates
            .iter()
            .find(|(_, c)| c.kind == CandidateKind::Host && c.addr == pkt.destination)
        {
            Some((id, _)) => id,
            None => {
                log::warn!(
                    "Failed to find matching local candidate for incoming STUN request ({})?",
                    pkt.destination
                );
                return;
            }
        };

        let matching_remote_candidate = self.remote_candidates.iter().find(|(_, c)| {
            // todo: also match protocol
            c.addr == pkt.source
        });

        let remote_id = match matching_remote_candidate {
            Some((remote, _)) => remote,
            None => {
                // No remote candidate with the source ip addr, create new peer-reflexive candidate
                let peer_reflexive_id = self.remote_candidates.insert(Candidate {
                    addr: pkt.source,
                    kind: CandidateKind::PeerReflexive,
                    priority: priority.0,
                    foundation: "~".into(),
                    component: pkt.component,
                    base: pkt.source,
                });

                // Pair it with the local candidate
                Self::add_candidate_pair(
                    local_id,
                    &self.local_candidates[local_id],
                    peer_reflexive_id,
                    &self.remote_candidates[peer_reflexive_id],
                    self.is_controlling,
                    &mut self.pairs,
                    false,
                );

                self.triggered_check_queue
                    .push_back((local_id, peer_reflexive_id));

                peer_reflexive_id
            }
        };

        let pair = self
            .pairs
            .iter_mut()
            .find(|p| p.local == local_id && p.remote == remote_id)
            .expect("local_id & remote_id are valid");

        pair.received_use_candidate = use_candidate;
        log::trace!(
            "got connectivity check for pair {}",
            DisplayPair(
                &self.local_candidates[pair.local],
                &self.remote_candidates[pair.remote],
            )
        );

        let stun_response = stun::make_success_response(
            stun_msg.transaction_id(),
            &self.local_credentials,
            pkt.source,
        );

        on_event(IceEvent::SendData {
            component: pair.component,
            data: stun_response,
            source: Some(self.local_candidates[local_id].base.ip()),
            target: pkt.source,
        });

        // Check nomination state if we received a use-candidate
        if use_candidate {
            self.poll_nomination(on_event);
        }
    }

    /// Drive the ICE agent forward. This must be called after the duration returned by [`timeout`](IceAgent::timeout).
    pub fn poll(&mut self, now: Instant, mut on_event: impl FnMut(IceEvent)) {
        // Handle pending stun retransmissions
        self.poll_retransmit(now, &mut on_event);

        for stun_server_bindings in &mut self.stun_server {
            stun_server_bindings.poll(now, &self.stun_config, &mut on_event);
        }

        self.poll_state(&mut on_event);

        // Skip anything beyond this before we received the remote credentials & candidates
        if self.remote_credentials.is_none() {
            return;
        }

        // Limit new checks to 1 per 50ms
        if let Some(it) = self.last_ta_trigger {
            if it + Duration::from_millis(50) > now {
                return;
            }
        }
        self.last_ta_trigger = Some(now);

        self.poll_nomination(&mut on_event);

        // If the triggered-check queue associated with the checklist
        // contains one or more candidate pairs, the agent removes the top
        // pair from the queue, performs a connectivity check on that pair,
        // puts the candidate pair state to In-Progress, and aborts the
        // subsequent steps.
        let pair = self
            .triggered_check_queue
            .pop_front()
            .and_then(|(local_id, remote_id)| {
                self.pairs
                    .iter_mut()
                    .find(|p| p.local == local_id && p.remote == remote_id)
            });

        let pair = if let Some(pair) = pair {
            Some(pair)
        } else {
            // If there are one or more candidate pairs in the Waiting state,
            // the agent picks the highest-priority candidate pair (if there are
            // multiple pairs with the same priority, the pair with the lowest
            // component ID is picked) in the Waiting state, performs a
            // connectivity check on that pair, puts the candidate pair state to
            // In-Progress, and aborts the subsequent steps.
            self.pairs
                .iter_mut()
                .find(|p| p.state == CandidatePairState::Waiting)
        };

        if let Some(pair) = pair {
            log::debug!(
                "start connectivity check for pair {}",
                DisplayPair(
                    &self.local_candidates[pair.local],
                    &self.remote_candidates[pair.remote]
                )
            );

            let transaction_id = TransactionId::random();

            let stun_request = stun::make_binding_request(
                transaction_id,
                &self.local_credentials,
                self.remote_credentials
                    .as_ref()
                    .expect("cannot make pairs without remote credentials"),
                &self.local_candidates[pair.local],
                self.is_controlling,
                self.control_tie_breaker,
                pair.nominated,
            );

            let source = self.local_candidates[pair.local].base.ip();
            let target = self.remote_candidates[pair.remote].addr;

            pair.state = CandidatePairState::InProgress {
                transaction_id,
                stun_request: stun_request.clone(),
                retransmit_at: now + self.stun_config.retransmit_delta(0),
                retransmits: 0,
                source,
                target,
            };

            on_event(IceEvent::SendData {
                component: pair.component,
                data: stun_request,
                source: Some(source),
                target,
            });
        }
    }

    fn poll_retransmit(&mut self, now: Instant, mut on_event: impl FnMut(IceEvent)) {
        for pair in &mut self.pairs {
            let CandidatePairState::InProgress {
                transaction_id: _,
                stun_request,
                retransmit_at,
                retransmits,
                source,
                target,
            } = &mut pair.state
            else {
                continue;
            };

            if *retransmit_at > now {
                continue;
            }

            if *retransmits >= self.stun_config.max_retransmits {
                pair.state = CandidatePairState::Failed;
                continue;
            }

            *retransmits += 1;
            *retransmit_at += self.stun_config.retransmit_delta(*retransmits);

            on_event(IceEvent::SendData {
                component: pair.component,
                data: stun_request.clone(),
                source: Some(*source),
                target: *target,
            });
        }
    }

    fn poll_state(&mut self, mut on_event: impl FnMut(IceEvent)) {
        // Check gathering state
        let mut all_completed = true;
        for stun_server in &self.stun_server {
            if !stun_server.completed() {
                all_completed = false;
            }
        }

        if all_completed && self.gathering_state != IceGatheringState::Complete {
            on_event(IceEvent::GatheringStateChanged {
                old: self.gathering_state,
                new: IceGatheringState::Complete,
            });

            self.gathering_state = IceGatheringState::Complete;
        } else if !all_completed && self.gathering_state != IceGatheringState::Gathering {
            on_event(IceEvent::GatheringStateChanged {
                old: self.gathering_state,
                new: IceGatheringState::Gathering,
            });

            self.gathering_state = IceGatheringState::Gathering;
        }

        // Check connection state
        let mut has_rtp_nomination = false;
        let mut has_rtcp_nomination = false;

        compile_error!("Check if there's still valid/waiting pairs, and set state to failed if all pairs have failed");

        for pair in &self.pairs {
            if pair.nominated && matches!(pair.state, CandidatePairState::Succeeded) {
                match pair.component {
                    Component::Rtp => has_rtp_nomination = true,
                    Component::Rtcp => has_rtcp_nomination = true,
                }
            }
        }

        let has_nomination_for_all = if self.rtcp_mux {
            has_rtp_nomination
        } else {
            has_rtp_nomination && has_rtcp_nomination
        };

        if has_nomination_for_all && self.connection_state != IceConnectionState::Connected {
            self.set_connection_state(IceConnectionState::Connected, on_event);
        } else if !has_nomination_for_all {
            match self.connection_state {
                IceConnectionState::New => {
                    self.set_connection_state(IceConnectionState::Checking, on_event);
                }
                IceConnectionState::Checking => {}
                IceConnectionState::Connected => {
                    self.set_connection_state(IceConnectionState::Disconnected, on_event);
                }
                IceConnectionState::Failed => {}
                IceConnectionState::Disconnected => {}
            }
        }
    }

    fn set_connection_state(
        &mut self,
        new: IceConnectionState,
        mut on_event: impl FnMut(IceEvent),
    ) {
        if self.connection_state != new {
            on_event(IceEvent::ConnectionStateChanged {
                old: self.connection_state,
                new,
            });
            self.connection_state = new;
        }
    }

    fn poll_nomination(&mut self, mut on_event: impl FnMut(IceEvent)) {
        self.poll_nomination_of_component(&mut on_event, Component::Rtp);

        if !self.rtcp_mux {
            self.poll_nomination_of_component(&mut on_event, Component::Rtcp);
        }
    }

    fn poll_nomination_of_component(
        &mut self,
        mut on_event: impl FnMut(IceEvent),
        component: Component,
    ) {
        if self.is_controlling {
            // Nothing to do, already nominated a pair
            let skip = self
                .pairs
                .iter()
                .any(|p| p.component == component && p.nominated);
            if skip {
                return;
            }

            let best_pair = self
                .pairs
                .iter_mut()
                .filter(|p| {
                    p.component == component && matches!(p.state, CandidatePairState::Succeeded)
                })
                .max_by_key(|p| p.priority);

            let Some(pair) = best_pair else {
                // no pair to nominate
                return;
            };

            log::debug!(
                "nominating {}",
                DisplayPair(
                    &self.local_candidates[pair.local],
                    &self.remote_candidates[pair.remote]
                )
            );

            pair.nominated = true;

            // Make another binding request with use-candidate as soon as possible, by pushing it to the front of the queue
            self.triggered_check_queue
                .push_front((pair.local, pair.remote));
        } else {
            // Not controlling, check if we have received a use-candidate for a successful pair

            // Skip this if we already have a nominated pair
            let skip = self.pairs.iter().any(|p| p.nominated);
            if skip {
                return;
            }

            // Find highest priority pair that received a use-candidate && was successful
            let pair = self
                .pairs
                .iter_mut()
                .filter(|p| {
                    p.component == component
                        && p.received_use_candidate
                        && matches!(p.state, CandidatePairState::Succeeded)
                })
                .max_by_key(|p| p.priority);

            let Some(pair) = pair else {
                // no pair to nominate
                return;
            };

            pair.nominated = true;

            on_event(IceEvent::UseAddr {
                component,
                target: self.remote_candidates[pair.remote].addr,
            });
        }
    }

    /// Returns a duration after which to call [`poll`](IceAgent::poll)
    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        // Next TA trigger
        let ta = self
            .last_ta_trigger
            .map(|it| {
                let poll_at = it + Duration::from_millis(50);
                poll_at.checked_duration_since(now)
            })
            .unwrap_or_default();

        // Next stun binding refresh/retransmit
        let stun_bindings = self.stun_server.iter().filter_map(|b| b.timeout(now)).min();

        opt_min(ta, stun_bindings)
    }

    /// Returns all discovered local ice agents, does not include peer-reflexive candidates
    pub fn ice_candidates(&self) -> Vec<IceCandidate> {
        self.local_candidates
            .values()
            .filter(|c| matches!(c.kind, CandidateKind::Host | CandidateKind::ServerReflexive))
            .map(|c| {
                let rel_addr = if c.kind == CandidateKind::ServerReflexive {
                    Some(c.base)
                } else {
                    None
                };

                IceCandidate {
                    foundation: c.foundation.clone().into(),
                    component: c.component as _,
                    transport: "UDP".into(),
                    priority: c.priority.into(),
                    address: UntaggedAddress::IpAddress(c.addr.ip()),
                    port: c.addr.port(),
                    typ: match c.kind {
                        CandidateKind::Host => "host".into(),
                        CandidateKind::ServerReflexive => "srflx".into(),
                        _ => unreachable!(),
                    },
                    rel_addr: rel_addr.map(|addr| UntaggedAddress::IpAddress(addr.ip())),
                    rel_port: rel_addr.map(|addr| addr.port()),
                    unknown: vec![],
                }
            })
            .collect()
    }
}

fn pair_priority(
    local_candidate: &Candidate,
    remote_candidate: &Candidate,
    is_controlling: bool,
) -> u64 {
    let (g, d) = if is_controlling {
        (
            local_candidate.priority as u64,
            remote_candidate.priority as u64,
        )
    } else {
        (
            remote_candidate.priority as u64,
            local_candidate.priority as u64,
        )
    };

    // pair priority = 2^32*MIN(G,D) + 2*MAX(G,D) + (G>D?1:0)
    2u64.pow(32) * min(g, d) + 2 * max(g, d) + if g > d { 1 } else { 0 }
}

fn compute_foundation(
    kind: CandidateKind,
    base: IpAddr,
    rel_addr: Option<IpAddr>,
    proto: &str,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    (kind, base, rel_addr, proto).hash(&mut hasher);
    hasher.finish()
}

struct DisplayPair<'a>(&'a Candidate, &'a Candidate);

impl fmt::Display for DisplayPair<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn fmt_candidate(f: &mut fmt::Formatter<'_>, c: &Candidate) -> fmt::Result {
            match c.kind {
                CandidateKind::Host => write!(f, "host({})", c.addr),
                CandidateKind::PeerReflexive => {
                    write!(f, "peer-reflexive(base:{}, peer:{})", c.base, c.addr)
                }
                CandidateKind::ServerReflexive => {
                    write!(f, "server-reflexive(base:{}, server:{})", c.base, c.addr)
                } // CandidateKind::Relayed => write!(f, "relayed(base:{}, relay:{})", c.base, c.addr),
            }
        }

        fmt_candidate(f, self.0)?;
        write!(f, " <-> ")?;
        fmt_candidate(f, self.1)
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
