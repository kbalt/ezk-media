use bytesstr::BytesStr;
use core::hash;
use sdp_types::{IceCandidate, UntaggedAddress};
use slotmap::{new_key_type, SlotMap};
use std::{
    cmp::{max, min},
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use crate::transport::{SocketUse, TransportEvent};

new_key_type!(
    struct CandidateId;
);

const BASE_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

pub enum IceEvent {
    CandidateGatheringComplete,
    SendData {
        socket: SocketUse,
        data: Vec<u8>,
        target: SocketAddr,
    },
}

pub struct IceAgent {
    remote_credentials: IceCredentials,

    local_candidates: SlotMap<CandidateId, Candidate>,
    remote_candidates: SlotMap<CandidateId, Candidate>,

    checklists: Checklist,

    stun_server: Option<SocketAddr>,

    is_controlling: bool,
}

struct Checklist {
    state: ChecklistState,

    max_pairs: usize,
    pairs: Vec<CandidatePair>,
}

enum ChecklistState {
    Running,
    Completed,
    Failed,
}

struct CandidatePair {
    local: CandidateId,
    remote: CandidateId,
    priority: u64,
}

enum CandidatePairState {
    Waiting,
    InProgress,
    Succeeded,
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
    priority: u64,
    foundation: String,

    /// In the ICE world this would be the component, here we're just tracking RTP/RTCP
    socket: SocketUse,

    // The transport address that an ICE agent sends from for a particular candidate.
    // For host, server-reflexive, and peer-reflexive candidates, the base is the same as the host candidate.
    // For relayed candidates, the base is the same as the relayed candidate
    //  (i.e., the transport address used by the TURN server to send from).
    base: SocketAddr,
}

pub struct IceCredentials {
    ufrag: String,
    pwd: String,
}

impl IceAgent {
    pub fn new(rtcp_mux: bool, is_controlling: bool, remote_credentials: IceCredentials) -> Self {
        IceAgent {
            remote_credentials,
            local_candidates: SlotMap::with_key(),
            remote_candidates: SlotMap::with_key(),
            checklists: Checklist {
                state: ChecklistState::Running,
                max_pairs: 100,
                pairs: Vec::new(),
            },
            stun_server: None,
            is_controlling,
        }
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
            .count() as u64
            + local_preference_offset;

        let kind_preference = 2u64.pow(24) * kind as u64;
        let local_preference = 2u64.pow(8) * local_preference;
        let priority = kind_preference + local_preference + (256 - socket as u64);

        // TODO: change this when adding server reflexive candidates
        let base = addr;

        self.local_candidates.insert(Candidate {
            addr,
            kind,
            priority,
            foundation: compute_foundation(kind, base, None, "udp").to_string(),
            socket,
            base,
        });
    }

    pub fn add_remote_candidtae(&mut self, candidate: IceCandidate) {
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
            priority: candidate.priority,
            foundation: candidate.foundation.parse().unwrap(),
            socket,
            base: SocketAddr::new(ip, candidate.port), // TODO: do I even need this?
        });
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
                    (local_candidate.priority, remote_candidate.priority)
                } else {
                    (remote_candidate.priority, local_candidate.priority)
                };

                // pair priority = 2^32*MIN(G,D) + 2*MAX(G,D) + (G>D?1:0)
                let priority = 2u64.pow(32) * min(g, d) + 2 * max(g, d) + if g > d { 1 } else { 0 };

                self.checklists.pairs.push(CandidatePair {
                    local: local_id,
                    remote: remote_id,
                    priority,
                })
            }
        }

        self.checklists.pairs.sort_unstable_by_key(|p| p.priority);
    }

    /// Prune the lowest priority pairs until `max_pairs` is reached
    fn prune_pairs(&mut self) {
        while self.checklists.pairs.len() > self.checklists.max_pairs {
            // TODO: is this enough?
            self.checklists.pairs.pop();
        }
    }

    /// Receive network packets for this ICE agent
    pub fn receive(&mut self, data: &[u8], source: SocketAddr, socket: SocketUse) {
        let x = stun_types::parse::ParsedMessage::parse(data.to_vec()).unwrap();

        // stun_types::attributes::
    }

    pub fn poll(&mut self) {}
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

// v=0
// o=mozilla...THIS_IS_SDPARTA-99.0 4914015295632667792 0 IN IP4 0.0.0.0
// s=-
// t=0 0
// a=fingerprint:sha-256 34:FF:83:E3:06:26:D9:DB:6C:C2:93:53:5F:EF:D0:AC:96:97:B7:F3:22:1E:53:AD:C9:04:06:A5:85:A8:E8:74
// a=group:BUNDLE 0 1 2
// a=ice-options:trickle
// a=msid-semantic:WMS *
// m=audio 9 UDP/TLS/RTP/SAVPF 109 9 0 8 101
// c=IN IP4 0.0.0.0
// a=sendrecv
// a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
// a=extmap:2/recvonly urn:ietf:params:rtp-hdrext:csrc-audio-level
// a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
// a=fmtp:109 maxplaybackrate=48000;stereo=1;useinbandfec=1
// a=fmtp:101 0-15
// a=ice-pwd:bf23a6064b33ac5be53b10b697aa8bca
// a=ice-ufrag:e57484d3
// a=mid:0
// a=msid:{d0748647-8527-4c64-bcef-8af6b51828ef} {04aa55c1-6e15-4107-923b-04db42a1b486}
// a=rtcp-mux
// a=rtpmap:109 opus/48000/2
// a=rtpmap:9 G722/8000/1
// a=rtpmap:0 PCMU/8000
// a=rtpmap:8 PCMA/8000
// a=rtpmap:101 telephone-event/8000/1
// a=setup:actpass
// a=ssrc:4264703613 cname:{0199e663-f620-4afe-a45b-654e0aa53e55}
// m=video 9 UDP/TLS/RTP/SAVPF 120 124 121 125 126 127 97 98 123 122 119
// c=IN IP4 0.0.0.0
// a=sendrecv
// a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
// a=extmap:4 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time
// a=extmap:5 urn:ietf:params:rtp-hdrext:toffset
// a=extmap:6/recvonly http://www.webrtc.org/experiments/rtp-hdrext/playout-delay
// a=extmap:7 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01
// a=fmtp:126 profile-level-id=42e01f;level-asymmetry-allowed=1;packetization-mode=1
// a=fmtp:97 profile-level-id=42e01f;level-asymmetry-allowed=1
// a=fmtp:120 max-fs=12288;max-fr=60
// a=fmtp:124 apt=120
// a=fmtp:121 max-fs=12288;max-fr=60
// a=fmtp:125 apt=121
// a=fmtp:127 apt=126
// a=fmtp:98 apt=97
// a=fmtp:119 apt=122
// a=ice-pwd:bf23a6064b33ac5be53b10b697aa8bca
// a=ice-ufrag:e57484d3
// a=mid:1
// a=msid:{d0748647-8527-4c64-bcef-8af6b51828ef} {662a1248-9683-43a8-a756-82aa5fb24eb7}
// a=rtcp-fb:120 nack
// a=rtcp-fb:120 nack pli
// a=rtcp-fb:120 ccm fir
// a=rtcp-fb:120 goog-remb
// a=rtcp-fb:120 transport-cc
// a=rtcp-fb:121 nack
// a=rtcp-fb:121 nack pli
// a=rtcp-fb:121 ccm fir
// a=rtcp-fb:121 goog-remb
// a=rtcp-fb:121 transport-cc
// a=rtcp-fb:126 nack
// a=rtcp-fb:126 nack pli
// a=rtcp-fb:126 ccm fir
// a=rtcp-fb:126 goog-remb
// a=rtcp-fb:126 transport-cc
// a=rtcp-fb:97 nack
// a=rtcp-fb:97 nack pli
// a=rtcp-fb:97 ccm fir
// a=rtcp-fb:97 goog-remb
// a=rtcp-fb:97 transport-cc
// a=rtcp-fb:123 nack
// a=rtcp-fb:123 nack pli
// a=rtcp-fb:123 ccm fir
// a=rtcp-fb:123 goog-remb
// a=rtcp-fb:123 transport-cc
// a=rtcp-fb:122 nack
// a=rtcp-fb:122 nack pli
// a=rtcp-fb:122 ccm fir
// a=rtcp-fb:122 goog-remb
// a=rtcp-fb:122 transport-cc
// a=rtcp-mux
// a=rtcp-rsize
// a=rtpmap:120 VP8/90000
// a=rtpmap:124 rtx/90000
// a=rtpmap:121 VP9/90000
// a=rtpmap:125 rtx/90000
// a=rtpmap:126 H264/90000
// a=rtpmap:127 rtx/90000
// a=rtpmap:97 H264/90000
// a=rtpmap:98 rtx/90000
// a=rtpmap:123 ulpfec/90000
// a=rtpmap:122 red/90000
// a=rtpmap:119 rtx/90000
// a=setup:actpass
// a=ssrc:2400956597 cname:{0199e663-f620-4afe-a45b-654e0aa53e55}
// a=ssrc:1488978004 cname:{0199e663-f620-4afe-a45b-654e0aa53e55}
// a=ssrc-group:FID 2400956597 1488978004
// m=application 9 UDP/DTLS/SCTP webrtc-datachannel
// c=IN IP4 0.0.0.0
// a=sendrecv
// a=ice-pwd:bf23a6064b33ac5be53b10b697aa8bca
// a=ice-ufrag:e57484d3
// a=mid:2
// a=setup:actpass
// a=sctp-port:5000
// a=max-message-size:1073741823
