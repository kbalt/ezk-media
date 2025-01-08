use bytesstr::BytesStr;
use sdp_types::{IceCandidate, UntaggedAddress};
use std::{collections::HashMap, net::SocketAddr};

pub struct IceAgent {
    next_id: u32,
    local_candidates: Vec<Candidate>,

    remote_candidates: Vec<IceCandidate>,

    checklists: HashMap<u32, Checklist>,

    // TODO: stun/turn server
    is_controlling: bool,
}

struct Checklist {
    state: ChecklistState,
}

enum ChecklistState {
    Running,
    Completed,
    Failed,
}

impl IceAgent {
    pub fn new(is_controlling: bool) -> Self {
        IceAgent {
            next_id: 0,
            local_candidates: vec![],
            remote_candidates: vec![],
            checklists: HashMap::new(),
            is_controlling,
        }
    }

    pub fn add_local_addr(&mut self, component: u32, addr: SocketAddr) {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            return;
        }

        if let SocketAddr::V6(v6) = addr {
            if v6.ip().to_ipv4().is_some() || v6.ip().to_ipv4_mapped().is_some() {
                return;
            }
        }

        let id = self.next_id;
        self.next_id += 1;

        let kind = CandidateKind::Host;

        // Calculate the candidate priority (trick that I have stolen from str0m's implementation (thank you o/))
        let local_preference_offset = match kind {
            CandidateKind::Host => (65535 / 4) * 3,
            CandidateKind::PeerReflexive => (65535 / 4) * 2,
            CandidateKind::ServerReflexive => 65535 / 4,
            CandidateKind::Relayed => 0,
        };

        let local_preference = local_preference_offset
            + self
                .local_candidates
                .iter()
                .filter(|c| c.kind == kind)
                .count();

        let kind_preference = 2u64.pow(24) * kind as u64;
        let local_preference = 2u64.pow(8) * local_preference as u64;
        let priority = kind_preference + local_preference + (256 - component as u64);

        self.local_candidates.push(Candidate {
            kind,
            priority,
            foundation: id,
            addr,
        });
    }

    pub fn add_remote_candidtae(&mut self, candidate: IceCandidate) {
        self.remote_candidates.push(candidate);
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[repr(u64)]
enum CandidateKind {
    Host = 126,
    PeerReflexive = 110,
    ServerReflexive = 100,
    Relayed = 0,
}

struct Candidate {
    kind: CandidateKind,
    priority: u64,
    foundation: u32,
    addr: SocketAddr,
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
