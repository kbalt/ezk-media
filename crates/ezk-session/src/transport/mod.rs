use crate::{rtp::RtpExtensionIds, ConnectionState, Error, TransportId, TransportType};
use dtls_srtp::{DtlsCertificate, DtlsSetup, DtlsSrtpSession};
use sdp_types::{Fingerprint, MediaDescription, Setup, SrtpCrypto, TransportProtocol};
use std::{borrow::Cow, collections::VecDeque, io, net::SocketAddr, time::Duration};

mod dtls_srtp;
mod packet_kind;
mod sdes_srtp;

pub(crate) use packet_kind::PacketKind;

#[derive(Default)]
pub(crate) struct SessionTransportState {
    /// DTLS certificate to use for all DTLS traffic in a session
    dtls_cert: Option<DtlsCertificate>,
}

pub(crate) enum TransportEvent {
    ConnectionState {
        old: ConnectionState,
        new: ConnectionState,
    },
    SendData {
        socket: SocketUse,
        data: Vec<u8>,
        target: SocketAddr,
    },
}

pub(crate) struct Transport {
    pub(crate) local_rtp_port: Option<u16>,
    pub(crate) local_rtcp_port: Option<u16>,

    pub(crate) remote_rtp_address: SocketAddr,
    pub(crate) remote_rtcp_address: SocketAddr,

    pub(crate) extension_ids: RtpExtensionIds,

    state: ConnectionState,
    kind: TransportKind,

    // Transport keep track of their own events separatly since they shouldn't be responsible for propagating these
    // events to the media tracks/streams that are using them.
    events: VecDeque<TransportEvent>,
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

impl Transport {
    pub(crate) fn create_from_offer(
        state: &mut SessionTransportState,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Option<Self>, Error> {
        match &remote_media_desc.media.proto {
            TransportProtocol::RtpAvp => Ok(Some(Self {
                local_rtp_port: None,
                local_rtcp_port: None,
                remote_rtp_address,
                remote_rtcp_address,
                extension_ids: RtpExtensionIds::from_offer(remote_media_desc),
                state: ConnectionState::Connected,
                kind: TransportKind::Rtp,
                events: VecDeque::from([TransportEvent::ConnectionState {
                    old: ConnectionState::New,
                    new: ConnectionState::Connected,
                }]),
            })),
            TransportProtocol::RtpSavp => {
                let (crypto, inbound, outbound) =
                    sdes_srtp::negotiate_sdes_srtp(&remote_media_desc.crypto)?;

                Ok(Some(Self {
                    local_rtp_port: None,
                    local_rtcp_port: None,
                    remote_rtp_address,
                    remote_rtcp_address,
                    extension_ids: RtpExtensionIds::from_offer(remote_media_desc),
                    state: ConnectionState::Connected,
                    kind: TransportKind::SdesSrtp {
                        crypto,
                        inbound,
                        outbound,
                    },
                    events: VecDeque::from([TransportEvent::ConnectionState {
                        old: ConnectionState::New,
                        new: ConnectionState::Connected,
                    }]),
                }))
            }
            TransportProtocol::UdpTlsRtpSavp => Some(Self::dtls_srtp(
                state,
                remote_media_desc,
                remote_rtp_address,
                remote_rtcp_address,
            ))
            .transpose(),
            _ => Ok(None),
        }
    }

    pub(crate) fn dtls_srtp(
        state: &mut SessionTransportState,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Self, Error> {
        if !remote_media_desc.rtcp_mux {
            return Err(io::Error::other("DTLS-SRTP without rtcp-mux is not supported").into());
        }

        let setup = match remote_media_desc.setup {
            Some(Setup::Active) => DtlsSetup::Accept,
            Some(Setup::Passive) => DtlsSetup::Connect,
            Some(Setup::ActPass) => {
                // Use passive when accepting an offer so both sides will have the DTLS fingerprint
                // before any request is sent
                DtlsSetup::Accept
            }
            Some(Setup::HoldConn) | None => {
                return Err(io::Error::other("missing or invalid setup attribute").into());
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

        let mut dtls = DtlsSrtpSession::new(
            state
                .dtls_cert
                .get_or_insert_with(DtlsCertificate::generate),
            remote_fingerprints,
            setup,
        )?;

        // Call handshake so that intial messages can be created
        assert!(dtls.handshake().unwrap().is_none());

        // Check for any send event from openssl
        let mut events = VecDeque::new();
        while let Some(data) = dtls.pop_to_send() {
            events.push_back(TransportEvent::SendData {
                socket: SocketUse::Rtp,
                data,
                target: remote_rtp_address,
            });
        }

        Ok(Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            remote_rtp_address,
            remote_rtcp_address,
            extension_ids: RtpExtensionIds::from_offer(remote_media_desc),
            state: ConnectionState::New,
            kind: TransportKind::DtlsSrtp {
                fingerprint: vec![dtls.fingerprint()],
                setup: match setup {
                    DtlsSetup::Accept => Setup::Passive,
                    DtlsSetup::Connect => Setup::Active,
                },
                dtls,
                srtp: None,
            },
            events,
        })
    }

    pub(crate) fn sdp_type(&self) -> TransportProtocol {
        match &self.kind {
            TransportKind::Rtp => TransportProtocol::RtpAvp,
            TransportKind::SdesSrtp { .. } => TransportProtocol::RtpSavp,
            TransportKind::DtlsSrtp { .. } => TransportProtocol::UdpTlsRtpSavp,
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

    pub(crate) fn type_(&self) -> TransportType {
        match self.kind {
            TransportKind::Rtp => TransportType::Rtp,
            TransportKind::SdesSrtp { .. } => TransportType::SdesSrtp,
            TransportKind::DtlsSrtp { .. } => TransportType::DtlsSrtp,
        }
    }

    pub(crate) fn populate_offer(&self, offer: &mut MediaDescription) {
        match &self.kind {
            TransportKind::Rtp => {}
            TransportKind::SdesSrtp { crypto, .. } => {
                offer.crypto.extend_from_slice(crypto);
            }
            TransportKind::DtlsSrtp {
                fingerprint, setup, ..
            } => {
                offer.setup = Some(*setup);
                offer.fingerprint.extend_from_slice(fingerprint);
            }
        }
    }

    pub(crate) fn timeout(&self) -> Option<Duration> {
        match &self.kind {
            TransportKind::Rtp => None,
            TransportKind::SdesSrtp { .. } => None,
            TransportKind::DtlsSrtp { dtls, .. } => dtls.timeout(),
        }
    }

    pub(crate) fn poll(&mut self) {
        match &mut self.kind {
            TransportKind::Rtp => {}
            TransportKind::SdesSrtp { .. } => {}
            TransportKind::DtlsSrtp { dtls, .. } => {
                assert!(dtls.handshake().unwrap().is_none());

                while let Some(data) = dtls.pop_to_send() {
                    self.events.push_back(TransportEvent::SendData {
                        socket: SocketUse::Rtp,
                        data,
                        target: self.remote_rtp_address,
                    });
                }
            }
        }
    }

    pub(crate) fn pop_event(&mut self) -> Option<TransportEvent> {
        self.events.pop_front()
    }

    pub(crate) fn receive(
        &mut self,
        data: &mut Cow<[u8]>,
        source: SocketAddr,
        socket: SocketUse,
    ) -> ReceivedPacket {
        match PacketKind::identify(data) {
            PacketKind::Rtp => {
                // Handle incoming RTP packet
                if let TransportKind::SdesSrtp { inbound, .. }
                | TransportKind::DtlsSrtp {
                    srtp: Some((inbound, _)),
                    ..
                } = &mut self.kind
                {
                    let data = data.to_mut();
                    inbound.unprotect(data).unwrap();
                }

                ReceivedPacket::Rtp
            }
            PacketKind::Rtcp => {
                // Handle incoming RTCP packet
                if let TransportKind::SdesSrtp { inbound, .. }
                | TransportKind::DtlsSrtp {
                    srtp: Some((inbound, _)),
                    ..
                } = &mut self.kind
                {
                    let data = data.to_mut();
                    inbound.unprotect_rtcp(data).unwrap();
                }

                ReceivedPacket::Rtcp
            }
            PacketKind::Stun => ReceivedPacket::TransportSpecific,
            PacketKind::Dtls => {
                if let TransportKind::DtlsSrtp { dtls, srtp, .. } = &mut self.kind {
                    dtls.receive(data.clone().into_owned());

                    if let Some((inbound, outbound)) = dtls.handshake().unwrap() {
                        if self.state != ConnectionState::Connected {
                            self.state = ConnectionState::Connected;
                            self.events.push_back(TransportEvent::ConnectionState {
                                old: self.state,
                                new: ConnectionState::Connected,
                            });
                        }

                        *srtp = Some((inbound.into_session(), outbound.into_session()));
                    }

                    while let Some(data) = dtls.pop_to_send() {
                        self.events.push_back(TransportEvent::SendData {
                            socket,
                            data,
                            target: source,
                        });
                    }
                }

                ReceivedPacket::TransportSpecific
            }
            PacketKind::Unknown => {
                // Discard
                ReceivedPacket::TransportSpecific
            }
        }
    }

    pub(crate) fn protect_rtp(&mut self, packet: &mut Vec<u8>) {
        if let TransportKind::SdesSrtp { outbound, .. }
        | TransportKind::DtlsSrtp {
            srtp: Some((_, outbound)),
            ..
        } = &mut self.kind
        {
            outbound.protect(packet).unwrap();
        }
    }

    pub(crate) fn protect_rtcp(&mut self, packet: &mut Vec<u8>) {
        if let TransportKind::SdesSrtp { outbound, .. }
        | TransportKind::DtlsSrtp {
            srtp: Some((_, outbound)),
            ..
        } = &mut self.kind
        {
            outbound.protect_rtcp(packet).unwrap();
        }
    }
}

/// Builder for a transport which has yet to be negotiated
pub(crate) struct TransportBuilder {
    local_rtp_port: Option<u16>,
    local_rtcp_port: Option<u16>,

    kind: TransportBuilderKind,

    backlog: Vec<(Vec<u8>, SocketAddr, SocketUse)>,
}

enum TransportBuilderKind {
    Rtp,
    SdesSrtp { crypto: Vec<SrtpCrypto> },
    DtlsSrtp { fingerprint: Vec<Fingerprint> },
}

impl TransportBuilder {
    pub(crate) fn new(state: &mut SessionTransportState, type_: TransportType) -> Self {
        match type_ {
            TransportType::Rtp => Self {
                local_rtp_port: None,
                local_rtcp_port: None,
                kind: TransportBuilderKind::Rtp,
                backlog: vec![],
            },
            TransportType::SdesSrtp => Self {
                local_rtp_port: None,
                local_rtcp_port: None,
                kind: TransportBuilderKind::SdesSrtp { crypto: todo!() },
                backlog: vec![],
            },
            TransportType::DtlsSrtp => {
                let cert = state
                    .dtls_cert
                    .get_or_insert_with(DtlsCertificate::generate);

                Self {
                    local_rtp_port: None,
                    local_rtcp_port: None,
                    kind: TransportBuilderKind::DtlsSrtp {
                        fingerprint: vec![cert.fingerprint()],
                    },
                    backlog: vec![],
                }
            }
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

    pub(crate) fn type_(&self) -> TransportType {
        match self.kind {
            TransportBuilderKind::Rtp => TransportType::Rtp,
            TransportBuilderKind::SdesSrtp { .. } => TransportType::SdesSrtp,
            TransportBuilderKind::DtlsSrtp { .. } => TransportType::DtlsSrtp,
        }
    }

    pub(crate) fn receive(&mut self, data: Vec<u8>, source: SocketAddr, socket: SocketUse) {
        self.backlog.push((data, source, socket));
    }

    pub(crate) fn build_from_answer(remote_media_desc: &MediaDescription) -> Transport {
        todo!()
    }
}

#[must_use]
pub(crate) enum ReceivedPacket {
    Rtp,
    Rtcp,
    TransportSpecific,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SocketUse {
    Rtp,
    Rtcp,
}

pub struct NewTransport<'a> {
    pub id: TransportId,
    pub rtcp_mux: bool,
    pub rtp_port: &'a mut Option<u16>,
    pub rtcp_port: &'a mut Option<u16>,
}
