use crate::{
    events::TransportRequiredChanges,
    ice::{IceAgent, IceCredentials, IceEvent},
    opt_min,
    rtp::RtpExtensionIds,
    ConnectionState, Error, ReceivedPkt, TransportType,
};
use dtls_srtp::{make_ssl_context, DtlsSetup, DtlsSrtpSession};
use openssl::{hash::MessageDigest, ssl::SslContext};
use sdp_types::{
    Fingerprint, FingerprintAlgorithm, MediaDescription, SessionDescription, Setup, SrtpCrypto,
    TransportProtocol,
};
use std::{collections::VecDeque, io, net::SocketAddr, time::Duration};

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

    pub(crate) ice_agent: Option<IceAgent>,

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
        session_desc: &SessionDescription,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Option<Self>, Error> {
        if remote_media_desc.rtcp_mux {
            required_changes.require_socket();
        } else {
            required_changes.require_socket_pair();
        }

        let ice_ufrag = session_desc
            .ice_ufrag
            .as_ref()
            .or(remote_media_desc.ice_ufrag.as_ref());

        let ice_pwd = session_desc
            .ice_pwd
            .as_ref()
            .or(remote_media_desc.ice_pwd.as_ref());

        let ice_agent = if let Some((ufrag, pwd)) = ice_ufrag.zip(ice_pwd) {
            let mut ice_agent = IceAgent::new(
                false,
                IceCredentials {
                    ufrag: ufrag.ufrag.to_string(),
                    pwd: pwd.pwd.to_string(),
                },
            );
            for candidate in &remote_media_desc.ice_candidates {
                ice_agent.add_remote_candidate(candidate);
            }

            Some(ice_agent)
        } else {
            None
        };

        let transport = match &remote_media_desc.media.proto {
            TransportProtocol::RtpAvp => Transport {
                local_rtp_port: None,
                local_rtcp_port: None,
                remote_rtp_address,
                remote_rtcp_address,
                rtcp_mux: remote_media_desc.rtcp_mux,
                ice_agent,
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
                    ice_agent,
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
                ice_agent,
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
        ice_agent: Option<IceAgent>,
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
            ice_agent,
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

        if let Some(ice_agent) = &self.ice_agent {
            desc.ice_candidates.extend(ice_agent.ice_candidates());
            desc.ice_ufrag = Some(sdp_types::IceUsernameFragment {
                ufrag: ice_agent.credentials().ufrag.clone().into(),
            });
            desc.ice_pwd = Some(sdp_types::IcePassword {
                pwd: ice_agent.credentials().pwd.clone().into(),
            });
            desc.ice_end_of_candidates = true;
        }
    }

    pub(crate) fn timeout(&self) -> Option<Duration> {
        let timeout = match &self.kind {
            TransportKind::Rtp => None,
            TransportKind::SdesSrtp { .. } => None,
            TransportKind::DtlsSrtp { dtls, .. } => dtls.timeout(),
        };

        if let Some(ice_agent) = &self.ice_agent {
            opt_min(ice_agent.timeout(), timeout)
        } else {
            timeout
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

        if let Some(ice_agent) = &mut self.ice_agent {
            ice_agent.poll(Self::handle_ice_event(
                &mut self.events,
                &mut self.remote_rtp_address,
                &mut self.remote_rtcp_address,
            ));
        }
    }

    /// Create a closure to handle events emitted by the IceAgent
    fn handle_ice_event<'a>(
        events: &'a mut VecDeque<TransportEvent>,
        remote_rtp_address: &'a mut SocketAddr,
        remote_rtcp_address: &'a mut SocketAddr,
    ) -> impl FnMut(IceEvent) + use<'a> {
        move |event| match event {
            IceEvent::UseAddr { socket, target } => match socket {
                SocketUse::Rtp => *remote_rtp_address = target,
                SocketUse::Rtcp => *remote_rtcp_address = target,
            },
            IceEvent::SendData {
                socket,
                data,
                target,
            } => {
                events.push_back(TransportEvent::SendData {
                    socket,
                    data,
                    target,
                });
            }
        }
    }

    pub(crate) fn pop_event(&mut self) -> Option<TransportEvent> {
        self.events.pop_front()
    }

    pub(crate) fn receive(&mut self, pkt: &mut ReceivedPkt) -> ReceivedPacket {
        match PacketKind::identify(&pkt.data) {
            PacketKind::Rtp => {
                // Handle incoming RTP packet
                if let TransportKind::SdesSrtp { inbound, .. }
                | TransportKind::DtlsSrtp {
                    srtp: Some((inbound, _)),
                    ..
                } = &mut self.kind
                {
                    inbound.unprotect(&mut pkt.data).unwrap();
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
                    inbound.unprotect_rtcp(&mut pkt.data).unwrap();
                }

                ReceivedPacket::Rtcp
            }
            PacketKind::Stun => {
                if let Some(ice_agent) = &mut self.ice_agent {
                    ice_agent.receive(
                        Self::handle_ice_event(
                            &mut self.events,
                            &mut self.remote_rtp_address,
                            &mut self.remote_rtcp_address,
                        ),
                        pkt,
                    );
                }

                ReceivedPacket::TransportSpecific
            }
            PacketKind::Dtls => {
                // We only expect DTLS traffic on the rtp socket
                if pkt.socket != SocketUse::Rtp {
                    return ReceivedPacket::TransportSpecific;
                }

                if let TransportKind::DtlsSrtp { dtls, srtp, .. } = &mut self.kind {
                    dtls.receive(pkt.data.clone());

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
                            socket: SocketUse::Rtp,
                            data,
                            target: self.remote_rtp_address,
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

#[derive(Debug)]
#[must_use]
pub(crate) enum ReceivedPacket {
    Rtp,
    Rtcp,
    TransportSpecific,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SocketUse {
    Rtp = 1,
    Rtcp = 2,
}
