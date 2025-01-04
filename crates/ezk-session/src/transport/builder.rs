use super::{
    dtls_srtp::{to_openssl_digest, DtlsCertificate, DtlsSetup, DtlsSrtpSession},
    sdes_srtp::{self, SdesSrtpOffer},
    NewTransport, ReceivedPacket, SessionTransportState, SocketUse, Transport, TransportEvent,
    TransportKind,
};
use crate::{rtp::RtpExtensionIds, ConnectionState, TransportId, TransportType};
use core::panic;
use sdp_types::{Fingerprint, MediaDescription, Setup};
use std::{borrow::Cow, collections::VecDeque, net::SocketAddr};

/// Builder for a transport which has yet to be negotiated
pub(crate) struct TransportBuilder {
    pub(crate) local_rtp_port: Option<u16>,
    pub(crate) local_rtcp_port: Option<u16>,

    kind: TransportBuilderKind,

    // Backlog of messages received before the SDP answer has been received
    backlog: Vec<(Vec<u8>, SocketAddr, SocketUse)>,
}

enum TransportBuilderKind {
    Rtp,
    SdesSrtp(SdesSrtpOffer),
    DtlsSrtp { fingerprint: Vec<Fingerprint> },
}

impl TransportBuilder {
    pub(crate) fn new(state: &mut SessionTransportState, type_: TransportType) -> Self {
        let kind = match type_ {
            TransportType::Rtp => TransportBuilderKind::Rtp,
            TransportType::SdesSrtp => {
                TransportBuilderKind::SdesSrtp(sdes_srtp::SdesSrtpOffer::new())
            }
            TransportType::DtlsSrtp => {
                let cert = state
                    .dtls_cert
                    .get_or_insert_with(DtlsCertificate::generate);

                TransportBuilderKind::DtlsSrtp {
                    fingerprint: vec![cert.fingerprint()],
                }
            }
        };

        Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            kind,
            backlog: vec![],
        }
    }

    pub(crate) fn as_new_transport(&mut self, id: TransportId) -> NewTransport<'_> {
        NewTransport {
            id,
            rtcp_mux: false,
            rtp_port: &mut self.local_rtp_port,
            rtcp_port: &mut self.local_rtcp_port,
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

    pub(crate) fn receive(&mut self, data: Vec<u8>, source: SocketAddr, socket: SocketUse) {
        // Limit the backlog buffer so it doesn't become a problem
        // this will never ever happen in a well behaved environment
        if self.backlog.len() > 100 {
            return;
        }

        self.backlog.push((data, source, socket));
    }

    pub(crate) fn build_from_answer(
        self,
        state: &mut SessionTransportState,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Transport {
        let mut transport = match self.kind {
            TransportBuilderKind::Rtp => Transport {
                local_rtp_port: self.local_rtp_port,
                local_rtcp_port: self.local_rtcp_port,
                remote_rtp_address,
                remote_rtcp_address,
                extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                state: ConnectionState::Connected,
                kind: TransportKind::Rtp,
                events: VecDeque::from([TransportEvent::ConnectionState {
                    old: ConnectionState::New,
                    new: ConnectionState::Connected,
                }]),
            },
            TransportBuilderKind::SdesSrtp(offer) => {
                let (crypto, inbound, outbound) = offer.receive_answer(&remote_media_desc.crypto);

                Transport {
                    local_rtp_port: self.local_rtp_port,
                    local_rtcp_port: self.local_rtcp_port,
                    remote_rtp_address,
                    remote_rtcp_address,
                    extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                    state: ConnectionState::Connected,
                    kind: TransportKind::SdesSrtp {
                        crypto: vec![crypto],
                        inbound,
                        outbound,
                    },
                    events: VecDeque::from([TransportEvent::ConnectionState {
                        old: ConnectionState::New,
                        new: ConnectionState::Connected,
                    }]),
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

                let dtls = DtlsSrtpSession::new(
                    state.dtls_cert.as_mut().unwrap(),
                    remote_fingerprints,
                    setup,
                )
                .unwrap();

                Transport {
                    local_rtp_port: self.local_rtp_port,
                    local_rtcp_port: self.local_rtcp_port,
                    remote_rtp_address,
                    remote_rtcp_address,
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

        for (msg, source, socket) in self.backlog {
            match transport.receive(&mut Cow::Owned(msg), source, socket) {
                ReceivedPacket::Rtp => todo!("handle early rtp"),
                ReceivedPacket::Rtcp => todo!("handle early rtcp"),
                ReceivedPacket::TransportSpecific => {}
            };
        }

        transport
    }
}
