use crate::{transport::SocketUse, ReceivedPkt};
use rand::distributions::{Alphanumeric, DistString};
use sdp_types::{IceCandidate, UntaggedAddress};
use slotmap::{new_key_type, SlotMap};
use std::{
    borrow::Cow,
    cmp::{max, min},
    collections::VecDeque,
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use stun_types::{
    attributes::{
        Fingerprint, MessageIntegrity, MessageIntegrityKey, Priority, UseCandidate, Username,
    },
    Class, Message, TransactionId,
};

mod stun;

new_key_type!(
    struct CandidateId;
);

pub enum IceEvent {
    UseAddr {
        socket: SocketUse,
        target: SocketAddr,
    },
    SendData {
        socket: SocketUse,
        data: Vec<u8>,
        target: SocketAddr,
    },
}

pub struct IceAgent {
    local_credentials: IceCredentials,
    remote_credentials: IceCredentials,

    local_candidates: SlotMap<CandidateId, Candidate>,
    remote_candidates: SlotMap<CandidateId, Candidate>,

    checklists: Checklist,

    stun_server: Option<SocketAddr>,

    is_controlling: bool,
    control_tie_breaker: u64,

    last_ta_trigger: Option<Instant>,
}

struct Checklist {
    state: ChecklistState,

    max_pairs: usize,
    pairs: Vec<CandidatePair>,
    triggered_check_queue: VecDeque<()>,

    use_pair: Option<(CandidateId, CandidateId)>,
}

enum ChecklistState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, PartialEq, Clone, Copy, Hash)]
#[repr(u64)]
enum CandidateKind {
    Host = 126,
    PeerReflexive = 110,
    ServerReflexive = 100,
    Relayed = 0,
}

struct Candidate {
    addr: SocketAddr,
    // transport: udp
    kind: CandidateKind,
    priority: u32,
    foundation: String,

    /// In the ICE world this would be the component, here we're just tracking RTP/RTCP
    socket: SocketUse,

    // The transport address that an ICE agent sends from for a particular candidate.
    // For host, server-reflexive, and peer-reflexive candidates, the base is the same as the host candidate.
    // For relayed candidates, the base is the same as the relayed candidate
    //  (i.e., the transport address used by the TURN server to send from).
    base: SocketAddr,
}

pub(super) struct CandidatePair {
    local: CandidateId,
    remote: CandidateId,
    priority: u64,
    state: CandidatePairState,
}

#[derive(Debug, Clone, PartialEq)]
enum CandidatePairState {
    /// A check has not been sent for this pair, but the pair is not Frozen.
    Waiting,

    /// A check has been sent for this pair, but the transaction is in progress.
    InProgress {
        transaction_id: TransactionId,
        sent_at: Instant,
    },

    // A check has been sent for this pair, and it produced a successful result.
    Succeeded,

    /// A check has been sent for this pair, and it failed (a response to the check
    /// was never received, or a failure response was received).
    Failed,

    /// A check for this pair has not been sent, and it cannot be
    /// sent until the pair is unfrozen and moved into the Waiting state.
    Frozen,
}

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
    pub fn new(is_controlling: bool, remote_credentials: IceCredentials) -> Self {
        IceAgent {
            local_credentials: IceCredentials::random(),
            remote_credentials,
            local_candidates: SlotMap::with_key(),
            remote_candidates: SlotMap::with_key(),
            checklists: Checklist {
                state: ChecklistState::Running,
                max_pairs: 100,
                pairs: Vec::new(),
                triggered_check_queue: VecDeque::new(),
                use_pair: None,
            },
            stun_server: None,
            is_controlling,
            control_tie_breaker: rand::random(),
            last_ta_trigger: None,
        }
    }

    pub fn credentials(&self) -> &IceCredentials {
        &self.local_credentials
    }

    pub fn add_host_addr(&mut self, socket: SocketUse, addr: SocketAddr) {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            return;
        }

        if let SocketAddr::V6(v6) = addr {
            let ip = v6.ip();
            if ip.to_ipv4().is_some() || ip.to_ipv4_mapped().is_some() {
                return;
            }
        }

        self.add_local_candidate(socket, CandidateKind::Host, addr);
    }

    fn add_local_candidate(&mut self, socket: SocketUse, kind: CandidateKind, addr: SocketAddr) {
        // Calculate the candidate priority using offsets + count of candidates of the same type
        // (trick that I have stolen from str0m's implementation (thank you o/))
        let local_preference_offset = match kind {
            CandidateKind::Host => (65535 / 4) * 3,
            CandidateKind::PeerReflexive => (65535 / 4) * 2,
            CandidateKind::ServerReflexive => 65535 / 4,
            CandidateKind::Relayed => 0,
        };

        let local_preference = self
            .local_candidates
            .values()
            .filter(|c| c.kind == kind)
            .count() as u32
            + local_preference_offset;

        let kind_preference = (kind as u32) << 24;
        let local_preference = local_preference << 8;
        let priority = kind_preference + local_preference + (256 - socket as u32);

        // TODO: change this when adding server reflexive candidates
        let base = addr;

        self.local_candidates.insert(Candidate {
            addr,
            kind,
            priority,
            foundation: compute_foundation(kind, base.ip(), None, "udp").to_string(),
            socket,
            base,
        });

        self.form_pairs();
    }

    pub fn add_remote_candidate(&mut self, candidate: &IceCandidate) {
        let kind = match candidate.typ.as_str() {
            "host" => CandidateKind::Host,
            "srflx" => CandidateKind::ServerReflexive,
            _ => return,
        };

        let socket = match candidate.component {
            1 => SocketUse::Rtp,
            2 => SocketUse::Rtcp,
            _ => return,
        };

        let ip = match candidate.address {
            UntaggedAddress::Fqdn(..) => return,
            UntaggedAddress::IpAddress(ip_addr) => ip_addr,
        };

        self.remote_candidates.insert(Candidate {
            addr: SocketAddr::new(ip, candidate.port),
            kind,
            priority: u32::try_from(candidate.priority).unwrap(),
            foundation: candidate.foundation.parse().unwrap(),
            socket,
            base: SocketAddr::new(ip, candidate.port), // TODO: do I even need this?
        });

        self.form_pairs();
    }

    fn form_pairs(&mut self) {
        for (local_id, local_candidate) in &self.local_candidates {
            for (remote_id, remote_candidate) in &self.remote_candidates {
                // Exclude pairs with different ip version
                match (local_candidate.addr.ip(), remote_candidate.addr.ip()) {
                    (IpAddr::V4(..), IpAddr::V4(..)) => { /* ok */ }
                    // Only pair IPv6 addresses when either both or neither are link local addresses
                    (IpAddr::V6(l), IpAddr::V6(r))
                        if l.is_unicast_link_local() == r.is_unicast_link_local() =>
                    { /* ok */ }
                    _ => {
                        // Would make an invalid pair, skip
                        continue;
                    }
                }

                // Check if the pair already exists
                let already_exists = self
                    .checklists
                    .pairs
                    .iter()
                    .any(|pair| pair.local == local_id && pair.remote == remote_id);

                if already_exists {
                    continue;
                }

                let (g, d) = if self.is_controlling {
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
                let priority = 2u64.pow(32) * min(g, d) + 2 * max(g, d) + if g > d { 1 } else { 0 };

                self.checklists.pairs.push(CandidatePair {
                    local: local_id,
                    remote: remote_id,
                    priority,
                    state: CandidatePairState::Waiting,
                });
            }
        }

        self.checklists.pairs.sort_unstable_by_key(|p| p.priority);

        self.prune_pairs();
    }

    /// Prune the lowest priority pairs until `max_pairs` is reached
    fn prune_pairs(&mut self) {
        while self.checklists.pairs.len() > self.checklists.max_pairs {
            let pair = self.checklists.pairs.pop().unwrap();
            log::debug!("Pruned pair {:?}:{:?}", pair.local, pair.remote);
        }
    }

    /// Receive network packets for this ICE agent
    pub fn receive(&mut self, mut on_event: impl FnMut(IceEvent), pkt: &ReceivedPkt) {
        // TODO: avoid clone here, this should be free
        let mut stun_msg = Message::parse(pkt.data.clone()).unwrap();

        let passed_integrity_check = stun_msg
            .attribute_with::<MessageIntegrity>(&MessageIntegrityKey::new_raw(Cow::Borrowed(
                self.local_credentials.pwd.as_bytes(),
            )))
            .is_some_and(|r| r.is_ok());
        let passed_fingerprint_check = stun_msg
            .attribute::<Fingerprint>()
            .is_some_and(|r| r.is_ok());

        if !passed_fingerprint_check || !passed_integrity_check {
            // todo: return error response?
            return;
        }

        let local_candidate = match self
            .local_candidates
            .iter()
            .find(|(_, c)| c.kind == CandidateKind::Host && c.addr.ip() == pkt.destination)
        {
            Some((id, _)) => id,
            None => {
                // TODO: unlucky?
                return;
            }
        };

        match stun_msg.class() {
            Class::Request => self.receive_stun_request(on_event, pkt, stun_msg),
            Class::Indication => todo!(),
            Class::Success => self.receive_stun_success(on_event, pkt, stun_msg),
            Class::Error => todo!(),
        }
    }

    fn receive_stun_success(
        &mut self,
        mut on_event: impl FnMut(IceEvent),
        pkt: &ReceivedPkt,
        stun_msg: Message,
    ) {
    }

    fn receive_stun_request(
        &mut self,
        mut on_event: impl FnMut(IceEvent),
        pkt: &ReceivedPkt,
        mut stun_msg: Message,
    ) {
        let username = stun_msg.attribute::<Username>().unwrap().unwrap();
        let expected_username = format!(
            "{}:{}",
            self.local_credentials.ufrag, self.remote_credentials.ufrag
        );

        if username.0 != expected_username {
            return;
        }

        let priority = stun_msg.attribute::<Priority>().unwrap().unwrap();
        let use_candidate = stun_msg.attribute::<UseCandidate>().is_some();

        if self.is_controlling && use_candidate {
            panic!()
        }

        let matching_remote_candidate = self.remote_candidates.iter().find(|(_, c)| {
            // todo: also match protocol
            c.addr == pkt.source
        });

        let remote_candidate = match matching_remote_candidate {
            Some((remote, _)) => remote,
            None => {
                // TODO: filter out already known peer reflexive candidates

                self.remote_candidates.insert(Candidate {
                    addr: pkt.source,
                    kind: CandidateKind::PeerReflexive,
                    priority: priority.0,
                    foundation: "TODO".into(), // TODO: ?
                    socket: pkt.socket,
                    base: pkt.source,
                })
            }
        };

        let local_candidate = match self
            .local_candidates
            .iter()
            .find(|(_, c)| c.kind == CandidateKind::Host && c.addr.ip() == pkt.destination)
        {
            Some((id, _)) => id,
            None => {
                // TODO: unlucky?
                return;
            }
        };

        let Some(pair) = self
            .checklists
            .pairs
            .iter_mut()
            .find(|p| p.local == local_candidate && p.remote == remote_candidate)
        else {
            panic!()
        };

        println!("Found pair!");

        let stun_response = stun::make_success_response(
            stun_msg.transaction_id(),
            &self.local_credentials,
            &self.remote_credentials,
            pkt.source,
        );

        on_event(IceEvent::SendData {
            socket: SocketUse::Rtp,
            data: stun_response,
            target: pkt.source,
        });

        if use_candidate {
            self.checklists.use_pair = Some((local_candidate, remote_candidate));

            on_event(IceEvent::UseAddr {
                socket: SocketUse::Rtp,
                target: self.remote_candidates[remote_candidate].addr,
            });
        }
    }

    pub fn poll(&mut self, mut on_event: impl FnMut(IceEvent)) {
        // Limit checks to 1 per 50ms
        let now = Instant::now();
        if let Some(it) = self.last_ta_trigger {
            if it + Duration::from_millis(5000) > now {
                return;
            }
        }
        self.last_ta_trigger = Some(now);

        // If the triggered-check queue associated with the checklist
        // contains one or more candidate pairs, the agent removes the top
        // pair from the queue, performs a connectivity check on that pair,
        // puts the candidate pair state to In-Progress, and aborts the
        // subsequent steps.
        if let Some(triggered_check) = self.checklists.triggered_check_queue.pop_front() {
            return todo!();
        }

        // If there are one or more candidate pairs in the Waiting state,
        // the agent picks the highest-priority candidate pair (if there are
        // multiple pairs with the same priority, the pair with the lowest
        // component ID is picked) in the Waiting state, performs a
        // connectivity check on that pair, puts the candidate pair state to
        // In-Progress, and aborts the subsequent steps.
        let highest_waiting_pair = self
            .checklists
            .pairs
            .iter_mut()
            .find(|p| p.state == CandidatePairState::Waiting);

        if let Some(pair) = highest_waiting_pair {
            let transaction_id = TransactionId::random();

            pair.state = CandidatePairState::InProgress {
                transaction_id,
                sent_at: now,
            };

            let stun_request = stun::make_binding_request(
                transaction_id,
                &self.local_credentials,
                &self.remote_credentials,
                &self.local_candidates[pair.local],
                self.is_controlling,
                self.control_tie_breaker,
            );

            on_event(IceEvent::SendData {
                socket: SocketUse::Rtp,
                data: stun_request,
                target: self.remote_candidates[pair.remote].addr,
            });

            return;
        }

        // If this step is reached, no check could be performed for the
        // checklist that was picked.  So, without waiting for timer Ta to
        // expire again, select the next checklist in the Running state and
        // return to step #1.  If this happens for every single checklist in
        // the Running state, meaning there are no remaining candidate pairs
        // to perform connectivity checks for, abort these steps.
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.last_ta_trigger
            .map(|it| {
                let poll_at = it + Duration::from_millis(50);
                poll_at.checked_duration_since(Instant::now())
            })
            .unwrap_or_default()
    }

    pub fn ice_candidates(&self) -> Vec<IceCandidate> {
        self.local_candidates
            .values()
            .filter(|c| matches!(c.kind, CandidateKind::Host | CandidateKind::ServerReflexive))
            .map(|c| IceCandidate {
                foundation: c.foundation.clone().into(),
                component: c.socket as _,
                transport: "UDP".into(),
                priority: c.priority.into(),
                address: UntaggedAddress::IpAddress(c.addr.ip()),
                port: c.addr.port(),
                typ: match c.kind {
                    CandidateKind::Host => "host".into(),
                    CandidateKind::ServerReflexive => "srflx".into(),
                    _ => unreachable!(),
                },
                rel_addr: None,
                rel_port: None,
                unknown: vec![],
            })
            .collect()
    }
}

fn compute_foundation(
    kind: CandidateKind,
    base: IpAddr,
    rel_addr: Option<IpAddr>,
    proto: &str,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    kind.hash(&mut hasher);
    base.hash(&mut hasher);
    rel_addr.hash(&mut hasher);
    proto.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_timeout() {
        let ice_agent = IceAgent::new(true, IceCredentials::random());
        assert_eq!(ice_agent.timeout(), Some(Duration::ZERO));
    }
}
