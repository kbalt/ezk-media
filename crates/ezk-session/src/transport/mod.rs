use crate::{
    rtp::RtpExtensionIds, ActiveMediaId, ConnectionState, Error, Event, Events, SocketId,
    TransportId,
};
use dtls_srtp::{DtlsSetup, DtlsSrtpSession};
use sdp_types::{Fingerprint, MediaDescription, Setup, SrtpCrypto, TransportProtocol};
use std::{borrow::Cow, io, net::SocketAddr, time::Duration};

mod dtls_srtp;
mod packet_kind;
mod sdes_srtp;

pub(crate) use packet_kind::PacketKind;

compile_error!("track transport events per transport and them convert them to user facing events on a track level");

pub(crate) enum TransportEvent {
    ConnectionState {
        old: ConnectionState,
        new: ConnectionState,
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
        events: &mut Events,
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
        active_media_id: ActiveMediaId,
    ) -> Result<Option<Self>, Error> {
        match &remote_media_desc.media.proto {
            TransportProtocol::RtpAvp => {
                events.push(Event::ConnectionState {
                    media_id: active_media_id,
                    state: ConnectionState::Connected,
                });

                Ok(Some(Self::rtp(
                    remote_media_desc,
                    remote_rtp_address,
                    remote_rtcp_address,
                )))
            }
            TransportProtocol::RtpSavp => {
                events.push(Event::ConnectionState {
                    media_id: active_media_id,
                    state: ConnectionState::Connected,
                });

                Some(Self::sdes_srtp(
                    remote_media_desc,
                    remote_rtp_address,
                    remote_rtcp_address,
                ))
                .transpose()
            }
            TransportProtocol::UdpTlsRtpSavp => Some(Self::dtls_srtp(
                remote_media_desc,
                remote_rtp_address,
                remote_rtcp_address,
            ))
            .transpose(),
            _ => Ok(None),
        }
    }

    pub(crate) fn rtp(
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Self {
        Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            remote_rtp_address,
            remote_rtcp_address,
            extension_ids: RtpExtensionIds::from_offer(remote_media_desc),
            state: ConnectionState::Connected,
            kind: TransportKind::Rtp,
        }
    }

    pub(crate) fn sdes_srtp(
        remote_media_desc: &MediaDescription,
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
    ) -> Result<Self, Error> {
        let (crypto, inbound, outbound) =
            sdes_srtp::negotiate_sdes_srtp(&remote_media_desc.crypto)?;

        Ok(Self {
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
        })
    }

    pub(crate) fn dtls_srtp(
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

        // TODO: connect will not work, since there's no initial handshake call
        let dtls = DtlsSrtpSession::new(remote_fingerprints, setup)?;

        Ok(Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            remote_rtp_address,
            remote_rtcp_address,
            extension_ids: RtpExtensionIds::from_offer(remote_media_desc),
            state: ConnectionState::Disconnected,
            kind: TransportKind::DtlsSrtp {
                fingerprint: vec![dtls.fingerprint()],
                setup: match setup {
                    DtlsSetup::Accept => Setup::Passive,
                    DtlsSetup::Connect => Setup::Active,
                },
                dtls,
                srtp: None,
            },
        })
    }

    pub(crate) fn sdp_type(&self) -> TransportProtocol {
        match &self.kind {
            TransportKind::Rtp => TransportProtocol::RtpAvp,
            TransportKind::SdesSrtp { .. } => TransportProtocol::RtpSavp,
            TransportKind::DtlsSrtp { .. } => TransportProtocol::UdpTlsRtpSavp,
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

    pub(crate) fn poll(&mut self, id: TransportId, events: &mut Events) {
        match &mut self.kind {
            TransportKind::Rtp => {}
            TransportKind::SdesSrtp { .. } => {}
            TransportKind::DtlsSrtp { dtls, .. } => {
                dtls.handshake().unwrap();

                while let Some(data) = dtls.pop_to_send() {
                    events.push(Event::SendData {
                        socket: SocketId(id, crate::SocketUse::Rtp),
                        data,
                        target: self.remote_rtp_address,
                    });
                }
            }
        }
    }

    pub(crate) fn receive(
        &mut self,
        events: &mut Events,
        data: &mut Cow<[u8]>,
        source: SocketAddr,
        socket_id: SocketId,
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
                        *srtp = Some((inbound.into_session(), outbound.into_session()));
                    }

                    while let Some(data) = dtls.pop_to_send() {
                        events.push(Event::SendData {
                            socket: socket_id,
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

#[must_use]
pub(crate) enum ReceivedPacket {
    Rtp,
    Rtcp,
    TransportSpecific,
}
