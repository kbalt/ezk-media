use super::{
    dtls_srtp::{to_openssl_digest, DtlsSetup, DtlsSrtpSession},
    sdes_srtp::{self, SdesSrtpOffer},
    ReceivedPacket, SessionTransportState, Transport, TransportEvent, TransportKind,
    TransportRequiredChanges,
};
use crate::{
    ice::{IceAgentBuilder, IceCredentials},
    rtp::RtpExtensionIds,
    ConnectionState, ReceivedPkt, RtcpMuxPolicy, TransportType,
};
use core::panic;
use sdp_types::{Fingerprint, MediaDescription, SessionDescription, Setup};
use std::{collections::VecDeque, net::SocketAddr, time::Instant};

/// Builder for a transport which has yet to be negotiated
pub(crate) struct TransportBuilder {
    pub(crate) local_rtp_port: Option<u16>,
    pub(crate) local_rtcp_port: Option<u16>,

    kind: TransportBuilderKind,

    pub(crate) ice_agent: Option<IceAgentBuilder>,

    // Backlog of messages received before the SDP answer has been received
    backlog: Vec<ReceivedPkt>,
}

compile_error!(
    "transport builder now also needs an event queue... use callback just like with ice?"
);

enum TransportBuilderKind {
    Rtp,
    SdesSrtp(SdesSrtpOffer),
    DtlsSrtp { fingerprint: Vec<Fingerprint> },
}

impl TransportBuilder {
    pub(crate) fn placeholder() -> Self {
        Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            kind: TransportBuilderKind::Rtp,
            ice_agent: None,
            backlog: vec![],
        }
    }

    pub(crate) fn new(
        state: &mut SessionTransportState,
        mut required_changes: TransportRequiredChanges<'_>,
        type_: TransportType,
        rtcp_mux_policy: RtcpMuxPolicy,
        offer_ice: bool,
    ) -> Self {
        match rtcp_mux_policy {
            RtcpMuxPolicy::Negotiate => required_changes.require_socket_pair(),
            RtcpMuxPolicy::Require => required_changes.require_socket(),
        }

        let kind = match type_ {
            TransportType::Rtp => TransportBuilderKind::Rtp,
            TransportType::SdesSrtp => {
                TransportBuilderKind::SdesSrtp(sdes_srtp::SdesSrtpOffer::new())
            }
            TransportType::DtlsSrtp => TransportBuilderKind::DtlsSrtp {
                fingerprint: vec![state.dtls_fingerprint()],
            },
        };

        Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            ice_agent: offer_ice.then(|| IceAgentBuilder::new(true)),
            kind,
            backlog: vec![],
        }
    }

    pub(crate) fn populate_desc(&self, desc: &mut MediaDescription) {
        desc.extmap.extend(RtpExtensionIds::new().to_extmap());

        match &self.kind {
            TransportBuilderKind::Rtp => {}
            TransportBuilderKind::SdesSrtp(offer) => {
                offer.extend_crypto(&mut desc.crypto);
            }
            TransportBuilderKind::DtlsSrtp { fingerprint, .. } => {
                desc.setup = Some(Setup::ActPass);
                desc.fingerprint.extend_from_slice(fingerprint);
            }
        }
    }

    pub(crate) fn type_(&self) -> TransportType {
        match self.kind {
            TransportBuilderKind::Rtp => TransportType::Rtp,
            TransportBuilderKind::SdesSrtp { .. } => TransportType::SdesSrtp,
            TransportBuilderKind::DtlsSrtp { .. } => TransportType::DtlsSrtp,
        }
    }

    pub(crate) fn poll(&mut self) {
        if let Some(ice_agent) = &mut self.ice_agent {
            ice_agent.poll(Instant::now(), |event| {});
        }
    }

    pub(crate) fn receive(&mut self, pkt: ReceivedPkt) {
        if let Some(ice_agent) = &mut self.ice_agent {
            if ice_agent.receive(&pkt) {
                // message has been handled by the ice-agent, return
                return;
            }
        }

        // Limit the backlog buffer so it doesn't become a problem
        // this will never ever happen in a well behaved environment
        if self.backlog.len() > 100 {
            return;
        }

        self.backlog.push(pkt);
    }

    pub(crate) fn build_from_answer(
        mut self,
        state: &mut SessionTransportState,
        mut required_changes: TransportRequiredChanges<'_>,
        session_desc: &SessionDescription,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Transport {
        // Remove RTCP socket if the answer has rtcp-mux set
        if remote_media_desc.rtcp_mux && self.local_rtcp_port.is_some() {
            required_changes.remove_rtcp_socket();
            self.local_rtcp_port = None;
        }

        let ice_ufrag = session_desc
            .ice_ufrag
            .as_ref()
            .or(remote_media_desc.ice_ufrag.as_ref());

        let ice_pwd = session_desc
            .ice_pwd
            .as_ref()
            .or(remote_media_desc.ice_pwd.as_ref());

        let ice_agent =
            if let Some((ice_agent, (ufrag, pwd))) = self.ice_agent.zip(ice_ufrag.zip(ice_pwd)) {
                let mut ice_agent = ice_agent.build(IceCredentials {
                    ufrag: ufrag.ufrag.to_string(),
                    pwd: pwd.pwd.to_string(),
                });

                for candidate in &remote_media_desc.ice_candidates {
                    ice_agent.add_remote_candidate(candidate);
                }

                Some(ice_agent)
            } else {
                None
            };

        let mut transport = match self.kind {
            TransportBuilderKind::Rtp => Transport {
                local_rtp_port: self.local_rtp_port,
                local_rtcp_port: self.local_rtcp_port,
                remote_rtp_address,
                remote_rtcp_address,
                rtcp_mux: remote_media_desc.rtcp_mux,
                ice_agent,
                extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                state: ConnectionState::Connected,
                kind: TransportKind::Rtp,
                events: VecDeque::new(),
            },
            TransportBuilderKind::SdesSrtp(offer) => {
                let (crypto, inbound, outbound) = offer.receive_answer(&remote_media_desc.crypto);

                Transport {
                    local_rtp_port: self.local_rtp_port,
                    local_rtcp_port: self.local_rtcp_port,
                    remote_rtp_address,
                    remote_rtcp_address,
                    rtcp_mux: remote_media_desc.rtcp_mux,
                    ice_agent,
                    extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                    state: ConnectionState::Connected,
                    kind: TransportKind::SdesSrtp {
                        crypto: vec![crypto],
                        inbound,
                        outbound,
                    },
                    events: VecDeque::new(),
                }
            }
            TransportBuilderKind::DtlsSrtp { fingerprint } => {
                let setup = match remote_media_desc.setup {
                    Some(Setup::Active) => DtlsSetup::Accept,
                    Some(Setup::Passive) => DtlsSetup::Connect,
                    _ => panic!("missing or invalid setup attribute"),
                };

                let remote_fingerprints: Vec<_> = remote_media_desc
                    .fingerprint
                    .iter()
                    .filter_map(|e| Some((to_openssl_digest(&e.algorithm)?, e.fingerprint.clone())))
                    .collect();

                let dtls =
                    DtlsSrtpSession::new(state.ssl_context(), remote_fingerprints.clone(), setup)
                        .unwrap();

                Transport {
                    local_rtp_port: self.local_rtp_port,
                    local_rtcp_port: self.local_rtcp_port,
                    remote_rtp_address,
                    remote_rtcp_address,
                    rtcp_mux: remote_media_desc.rtcp_mux,
                    ice_agent,
                    extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                    state: ConnectionState::New,
                    kind: TransportKind::DtlsSrtp {
                        fingerprint,
                        setup: match setup {
                            DtlsSetup::Accept => Setup::Passive,
                            DtlsSetup::Connect => Setup::Active,
                        },
                        dtls,
                        srtp: None,
                    },
                    events: VecDeque::new(),
                }
            }
        };

        if transport.state != ConnectionState::New {
            transport.events.push_back(TransportEvent::ConnectionState {
                old: ConnectionState::New,
                new: transport.state,
            });
        }

        // Feed the already received messages into the transport
        for mut pkt in self.backlog {
            match transport.receive(&mut pkt) {
                ReceivedPacket::Rtp => todo!("handle early rtp"),
                ReceivedPacket::Rtcp => todo!("handle early rtcp"),
                ReceivedPacket::TransportSpecific => {}
            };
        }

        transport
    }
}
