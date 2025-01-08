use crate::{
    events::TransportRequiredChanges, rtp::RtpExtensionIds, ConnectionState, Error, TransportType,
};
use dtls_srtp::{make_ssl_context, DtlsSetup, DtlsSrtpSession};
use openssl::{hash::MessageDigest, ssl::SslContext};
use sdp_types::{
    Fingerprint, FingerprintAlgorithm, MediaDescription, Setup, SrtpCrypto, TransportProtocol,
};
use std::{borrow::Cow, collections::VecDeque, io, net::SocketAddr, time::Duration};

mod builder;
mod dtls_srtp;
mod packet_kind;
mod sdes_srtp;

pub(crate) use builder::TransportBuilder;
pub(crate) use packet_kind::PacketKind;

#[derive(Default)]
pub(crate) struct SessionTransportState {
    ssl_context: Option<openssl::ssl::SslContext>,
}

impl SessionTransportState {
    fn ssl_context(&mut self) -> &mut SslContext {
        self.ssl_context.get_or_insert_with(make_ssl_context)
    }

    fn dtls_fingerprint(&mut self) -> Fingerprint {
        let ctx = self.ssl_context();

        Fingerprint {
            algorithm: FingerprintAlgorithm::SHA256,
            fingerprint: ctx
                .certificate()
                .unwrap()
                .digest(MessageDigest::sha256())
                .unwrap()
                .to_vec(),
        }
    }
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

    pub(crate) rtcp_mux: bool,

    // TODO: either split these up in send / receive ids or just make then receive and always use RtpExtensionIds::new() for send
    pub(crate) extension_ids: RtpExtensionIds,

    state: ConnectionState,
    kind: TransportKind,

    // Transport keep track of their own events separately since they shouldn't be responsible for propagating these
    // events to the media tracks/streams that are using them.
    events: VecDeque<TransportEvent>,
}

enum TransportKind {
    Rtp,
    SdesSrtp {
        /// Local crypto attribute
        crypto: Vec<SrtpCrypto>,
        inbound: srtp::Session,
        outbound: srtp::Session,
    },
    DtlsSrtp {
        /// Local DTLS certificate fingerprint attribute
        fingerprint: Vec<Fingerprint>,
        setup: Setup,

        dtls: DtlsSrtpSession,
        srtp: Option<(srtp::Session, srtp::Session)>,
    },
}

impl Transport {
    // TODO: rethink the return type here, this Result<Option<T>> business isn't really working out on the caller site
    pub(crate) fn create_from_offer(
        state: &mut SessionTransportState,
        mut required_changes: TransportRequiredChanges<'_>,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Option<Self>, Error> {
        if remote_media_desc.rtcp_mux {
            required_changes.require_socket();
        } else {
            required_changes.require_socket_pair();
        }

        let transport = match &remote_media_desc.media.proto {
            TransportProtocol::RtpAvp => Transport {
                local_rtp_port: None,
                local_rtcp_port: None,
                remote_rtp_address,
                remote_rtcp_address,
                rtcp_mux: remote_media_desc.rtcp_mux,
                extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
                state: ConnectionState::Connected,
                kind: TransportKind::Rtp,
                events: VecDeque::from([TransportEvent::ConnectionState {
                    old: ConnectionState::New,
                    new: ConnectionState::Connected,
                }]),
            },
            TransportProtocol::RtpSavp => {
                let (crypto, inbound, outbound) =
                    sdes_srtp::negotiate_from_offer(&remote_media_desc.crypto)?;

                Transport {
                    local_rtp_port: None,
                    local_rtcp_port: None,
                    remote_rtp_address,
                    remote_rtcp_address,
                    rtcp_mux: remote_media_desc.rtcp_mux,
                    extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
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
                }
            }
            TransportProtocol::UdpTlsRtpSavp => Self::dtls_srtp_from_offer(
                state,
                remote_media_desc,
                remote_rtp_address,
                remote_rtcp_address,
            )?,
            _ => return Ok(None),
        };

        Ok(Some(transport))
    }

    pub(crate) fn dtls_srtp_from_offer(
        state: &mut SessionTransportState,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Self, Error> {
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

        let mut events = VecDeque::new();

        let mut dtls =
            DtlsSrtpSession::new(state.ssl_context(), remote_fingerprints.clone(), setup)?;
        while let Some(data) = dtls.pop_to_send() {
            events.push_back(TransportEvent::SendData {
                socket: SocketUse::Rtp,
                data,
                target: remote_rtp_address,
            });
        }

        Ok(Transport {
            local_rtp_port: None,
            local_rtcp_port: None,
            remote_rtp_address,
            remote_rtcp_address,
            rtcp_mux: remote_media_desc.rtcp_mux,
            extension_ids: RtpExtensionIds::from_desc(remote_media_desc),
            state: ConnectionState::New,
            kind: TransportKind::DtlsSrtp {
                fingerprint: vec![state.dtls_fingerprint()],
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

    pub(crate) fn type_(&self) -> TransportType {
        match self.kind {
            TransportKind::Rtp => TransportType::Rtp,
            TransportKind::SdesSrtp { .. } => TransportType::SdesSrtp,
            TransportKind::DtlsSrtp { .. } => TransportType::DtlsSrtp,
        }
    }

    pub(crate) fn populate_desc(&self, desc: &mut MediaDescription) {
        desc.extmap.extend(self.extension_ids.to_extmap());

        match &self.kind {
            TransportKind::Rtp => {}
            TransportKind::SdesSrtp { crypto, .. } => {
                desc.crypto.extend_from_slice(crypto);
            }
            TransportKind::DtlsSrtp {
                fingerprint, setup, ..
            } => {
                desc.setup = Some(*setup);
                desc.fingerprint.extend_from_slice(fingerprint);
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
                        Self::update_connection_state(
                            &mut self.events,
                            &mut self.state,
                            ConnectionState::Connected,
                        );

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
        match &mut self.kind {
            TransportKind::DtlsSrtp { srtp: None, .. } => {
                panic!("Tried to protect RTP on non-ready DTLS-SRTP transport");
            }
            TransportKind::SdesSrtp { outbound, .. }
            | TransportKind::DtlsSrtp {
                srtp: Some((_, outbound)),
                ..
            } => {
                outbound.protect(packet).unwrap();
            }
            _ => (),
        }
    }

    pub(crate) fn protect_rtcp(&mut self, packet: &mut Vec<u8>) {
        match &mut self.kind {
            TransportKind::DtlsSrtp { srtp: None, .. } => {
                panic!("Tried to protect RTCP on non-ready DTLS-SRTP transport");
            }
            TransportKind::SdesSrtp { outbound, .. }
            | TransportKind::DtlsSrtp {
                srtp: Some((_, outbound)),
                ..
            } => {
                outbound.protect_rtcp(packet).unwrap();
            }
            _ => (),
        }
    }

    // Set the a new connection state and emit an event if the state differs from the old one
    fn update_connection_state(
        events: &mut VecDeque<TransportEvent>,
        state: &mut ConnectionState,
        new: ConnectionState,
    ) {
        if *state != new {
            events.push_back(TransportEvent::ConnectionState {
                old: *state,
                new: ConnectionState::Connected,
            });

            *state = ConnectionState::Connected;
        }
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
