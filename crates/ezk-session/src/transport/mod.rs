use crate::{Error, Instruction, SocketId, RTP_MID_HDREXT};
use dtls_srtp::{DtlsSetup, DtlsSrtpSession};
use sdp_types::{Fingerprint, MediaDescription, Setup, SrtpCrypto, TransportProtocol};
use std::{borrow::Cow, collections::VecDeque, io, net::SocketAddr};

mod dtls_srtp;
mod packet_kind;
mod sdes_srtp;

pub(crate) use packet_kind::PacketKind;

pub(crate) struct Transport {
    pub(crate) local_rtp_port: Option<u16>,
    pub(crate) local_rtcp_port: Option<u16>,

    pub(crate) remote_rtp_address: SocketAddr,
    pub(crate) remote_rtcp_address: SocketAddr,

    kind: TransportKind,

    pub(crate) mid_rtp_id: Option<u8>,
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
            kind: TransportKind::Rtp,
            mid_rtp_id: rtp_mid_id(remote_media_desc),
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
            kind: TransportKind::SdesSrtp {
                crypto,
                inbound,
                outbound,
            },
            mid_rtp_id: rtp_mid_id(remote_media_desc),
        })
    }

    pub(crate) fn dtls_srtp(
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

        // TODO: connect will not work, since there's no initial handshake call
        let dtls = DtlsSrtpSession::new(remote_fingerprints, setup)?;

        Ok(Self {
            local_rtp_port: None,
            local_rtcp_port: None,
            remote_rtp_address,
            remote_rtcp_address,
            kind: TransportKind::DtlsSrtp {
                fingerprint: vec![dtls.fingerprint()],
                setup: match setup {
                    DtlsSetup::Accept => Setup::Passive,
                    DtlsSetup::Connect => Setup::Active,
                },
                dtls,
                srtp: None,
            },
            mid_rtp_id: rtp_mid_id(remote_media_desc),
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

    pub(crate) fn receive(
        &mut self,
        instructions: &mut VecDeque<Instruction>,
        data: &mut Cow<[u8]>,
        source: SocketAddr,
        socket_id: SocketId,
    ) -> ReceivedPacket {
        match PacketKind::identify(&data) {
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
                        instructions.push_back(Instruction::SendData {
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

    pub fn protect_rtp(&mut self, packet: &mut Vec<u8>) {
        if let TransportKind::SdesSrtp { outbound, .. }
        | TransportKind::DtlsSrtp {
            srtp: Some((_, outbound)),
            ..
        } = &mut self.kind
        {
            outbound.protect(packet).unwrap();
        }
    }

    pub fn protect_rtcp(&mut self, packet: &mut Vec<u8>) {
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

fn rtp_mid_id(remote_media_desc: &MediaDescription) -> Option<u8> {
    remote_media_desc
        .extmap
        .iter()
        .find(|extmap| extmap.uri == RTP_MID_HDREXT)
        .map(|extmap| extmap.id)
}
