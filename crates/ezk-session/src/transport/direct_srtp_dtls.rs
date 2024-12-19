use super::{
    dtls_srtp::{DtlsSrtpAcceptor, DtlsSrtpConnector},
    PacketKind, RtpTransport, RECV_BUFFER_SIZE,
};
use openssl::hash::MessageDigest;
use sdp_types::{Fingerprint, FingerprintAlgorithm};
use srtp::openssl::InboundSession;
use std::{
    io::{self},
    net::SocketAddr,
    sync::Arc,
};
use stun_types::{
    attributes::XorMappedAddress,
    builder::MessageBuilder,
    header::{Class, Method},
    parse::ParsedMessage,
};
use tokio::{net::UdpSocket, select};

pub struct DirectDtlsSrtpTransport {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,

    state: State,
}

enum State {
    Connecting(DtlsSrtpConnector),
    Accepting(DtlsSrtpAcceptor),
    Established {
        inbound: srtp::openssl::InboundSession,
        outbound: srtp::openssl::OutboundSession,
    },
}

pub enum DtlsSetup {
    Connect,
    Accept,
}

impl DirectDtlsSrtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
        remote_fingerprints: Vec<Fingerprint>,
        setup: DtlsSetup,
    ) -> io::Result<(Self, Vec<Fingerprint>)> {
        let rtp_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports

        let rtcp_socket = if remote_rtcp_address.is_some() {
            Some(Arc::new(UdpSocket::bind("0.0.0.0:0").await?))
        } else {
            None
        };

        let mut fingerprint = vec![];
        let state = match setup {
            DtlsSetup::Connect => State::Connecting(DtlsSrtpConnector::new(
                rtp_socket.clone(),
                remote_rtp_address,
                remote_fingerprints
                    .into_iter()
                    .filter_map(|e| Some((to_ssl_digest(&e.algorithm)?, e.fingerprint)))
                    .collect(),
            )?),
            DtlsSetup::Accept => {
                let acceptor = DtlsSrtpAcceptor::new(rtp_socket.clone(), remote_rtp_address)?;
                fingerprint.push(acceptor.fingerprint());
                State::Accepting(acceptor)
            }
        };

        Ok((
            Self {
                rtp_socket,
                rtcp_socket,
                remote_rtp_address,
                remote_rtcp_address,
                state,
            },
            fingerprint,
        ))
    }

    pub fn local_rtp_port(&self) -> u16 {
        self.rtp_socket.local_addr().unwrap().port()
    }

    pub fn local_rtcp_port(&self) -> Option<u16> {
        let rtcp_socket = self.rtcp_socket.as_ref()?;

        Some(rtcp_socket.local_addr().unwrap().port())
    }
}

impl RtpTransport for DirectDtlsSrtpTransport {
    async fn recv(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        // Loop until the DTLS-SRTP session has been established
        let inbound = loop {
            match &mut self.state {
                State::Connecting(connector) => {
                    let (inbound, outbound) = connector.connect().await?;
                    self.state = State::Established { inbound, outbound }
                }
                State::Accepting(acceptor) => {
                    let (inbound, outbound) = acceptor.accept().await?;
                    self.state = State::Established { inbound, outbound }
                }
                State::Established { inbound, .. } => break inbound,
            };
        };

        if let Some(rtcp_socket) = &self.rtcp_socket {
            // Poll both rtp_socket & rtcp_socket for readyness and try_read once available
            loop {
                select! {
                    result = self.rtp_socket.readable() => {
                        result?;
                        if try_recv(inbound, &self.rtp_socket, buf).await? {
                            return Ok(())
                        }
                    },
                    result = rtcp_socket.readable() => {
                        result?;
                        if try_recv(inbound, rtcp_socket, buf).await? {
                            return Ok(())
                        }
                    },
                }
            }
        }

        loop {
            // No rtcp_socket, just read from the rtp_socket
            let (len, remote) = self.rtp_socket.recv_from(buf).await?;

            buf.truncate(len);

            match PacketKind::identify(buf) {
                PacketKind::Rtp => {
                    inbound.unprotect(buf).map_err(io::Error::other)?;
                    return Ok(());
                }
                PacketKind::Rtcp => {
                    inbound.unprotect_rtcp(buf).map_err(io::Error::other)?;
                    return Ok(());
                }
                PacketKind::Stun => {
                    check_for_stun_binding_request(&self.rtp_socket, buf, remote).await?;
                    buf.resize(RECV_BUFFER_SIZE, 0);
                }
                PacketKind::Unknown => {
                    buf.resize(RECV_BUFFER_SIZE, 0);
                    continue;
                }
            }
        }
    }

    async fn send_rtp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let State::Established { outbound, .. } = &mut self.state else {
            return Err(io::Error::other("dtls-srtp not ready"));
        };

        outbound.protect(buf).map_err(io::Error::other)?;

        self.rtp_socket
            .send_to(buf, self.remote_rtp_address)
            .await?;

        Ok(())
    }

    async fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let State::Established { outbound, .. } = &mut self.state else {
            return Err(io::Error::other("dtls-srtp not ready"));
        };

        if let Some(rtcp_socket) = &self.rtcp_socket {
            outbound.protect_rtcp(buf).map_err(io::Error::other)?;

            rtcp_socket
                .send_to(
                    buf,
                    self.remote_rtcp_address.unwrap_or(self.remote_rtp_address),
                )
                .await?;

            Ok(())
        } else {
            self.send_rtp(buf).await
        }
    }

    fn is_ready(&self) -> bool {
        matches!(self.state, State::Established { .. })
    }
}

async fn try_recv(
    inbound: &mut InboundSession,
    socket: &UdpSocket,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    let (len, remote) = match socket.try_recv_from(buf) {
        Ok((len, remote)) => (len, remote),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
        Err(e) => return Err(e),
    };

    buf.truncate(len);

    match PacketKind::identify(buf) {
        PacketKind::Rtp => {
            inbound.unprotect(buf).map_err(io::Error::other)?;
            Ok(true)
        }
        PacketKind::Rtcp => {
            inbound.unprotect_rtcp(buf).map_err(io::Error::other)?;
            Ok(true)
        }
        PacketKind::Stun => {
            check_for_stun_binding_request(socket, buf, remote).await?;
            buf.resize(RECV_BUFFER_SIZE, 0);
            Ok(false)
        }
        PacketKind::Unknown => {
            buf.resize(RECV_BUFFER_SIZE, 0);
            Ok(false)
        }
    }
}

async fn check_for_stun_binding_request(
    socket: &UdpSocket,
    buf: &[u8],
    remote: SocketAddr,
) -> io::Result<()> {
    let Ok(e) = ParsedMessage::parse(buf.to_vec()) else {
        return Ok(());
    };

    if e.class == Class::Request && e.method == Method::Binding {
        let mut msg = MessageBuilder::new(Class::Success, Method::Binding, e.tsx_id);
        msg.add_attr(&XorMappedAddress(remote)).unwrap();
        let msg = msg.finish();
        socket.send_to(&msg, remote).await?;
    }

    Ok(())
}

fn to_ssl_digest(algo: &FingerprintAlgorithm) -> Option<MessageDigest> {
    match algo {
        FingerprintAlgorithm::SHA1 => Some(MessageDigest::sha1()),
        FingerprintAlgorithm::SHA224 => Some(MessageDigest::sha224()),
        FingerprintAlgorithm::SHA256 => Some(MessageDigest::sha256()),
        FingerprintAlgorithm::SHA384 => Some(MessageDigest::sha384()),
        FingerprintAlgorithm::SHA512 => Some(MessageDigest::sha512()),
        FingerprintAlgorithm::MD5 => Some(MessageDigest::md5()),
        FingerprintAlgorithm::MD2 => None,
        FingerprintAlgorithm::Other(..) => None,
    }
}
