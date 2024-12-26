use super::{
    dtls_srtp::DtlsSrtpSession, DtlsSetup, PacketKind, RtpTransport, WhichTransport,
    RECV_BUFFER_SIZE,
};
use openssl::hash::MessageDigest;
use sdp_types::{Fingerprint, FingerprintAlgorithm};
use std::{
    io::{self},
    net::SocketAddr,
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    select,
    time::{interval, Interval},
};

pub struct DirectDtlsSrtpTransport {
    rtp_socket: Socket,
    rtcp_socket: Option<Socket>,

    /// Workaround: poll openssl in an interval since there's currently no way get obtain timing information
    handshake_interval: Interval,
}

struct Socket {
    recv_buf: Vec<u8>,
    socket: UdpSocket,
    dtls: DtlsSrtpSession,
    srtp: Option<(srtp::Session, srtp::Session)>,
    target: SocketAddr,
}

impl Socket {
    async fn recv(&mut self) -> io::Result<SocketAddr> {
        let (len, source) = self.socket.recv_from(&mut self.recv_buf).await?;
        self.recv_buf.truncate(len);
        Ok(source)
    }

    async fn handle_recv(&mut self) {}
}

impl DirectDtlsSrtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
        remote_fingerprints: Vec<Fingerprint>,
        setup: DtlsSetup,
    ) -> io::Result<(Self, Vec<Fingerprint>)> {
        let remote_fingerprints: Vec<_> = remote_fingerprints
            .into_iter()
            .filter_map(|e| Some((to_ssl_digest(&e.algorithm)?, e.fingerprint)))
            .collect();

        // Setup RTP socket
        let rtp_socket = UdpSocket::bind("0.0.0.0:0").await?;
        let rtp_dtls = DtlsSrtpSession::new(remote_fingerprints.clone(), setup)?;
        let fingerprint = vec![rtp_dtls.fingerprint()];

        let rtp_socket = Socket {
            recv_buf: vec![0u8; RECV_BUFFER_SIZE],
            socket: rtp_socket,
            dtls: rtp_dtls,
            srtp: None,
            target: remote_rtp_address,
        };

        // Setup RTCP socket if required
        let rtcp_socket = if let Some(remote_rtcp_address) = remote_rtcp_address {
            let rtcp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            let rtcp_acceptor = DtlsSrtpSession::new(remote_fingerprints, setup)?;

            Some(Socket {
                recv_buf: vec![0u8; RECV_BUFFER_SIZE],
                socket: rtcp_socket,
                dtls: rtcp_acceptor,
                srtp: None,
                target: remote_rtcp_address,
            })
        } else {
            None
        };

        Ok((
            Self {
                rtp_socket,
                rtcp_socket,
                handshake_interval: interval(Duration::from_millis(10)),
            },
            fingerprint,
        ))
    }

    pub fn local_rtp_port(&self) -> u16 {
        self.rtp_socket.socket.local_addr().unwrap().port()
    }

    pub fn local_rtcp_port(&self) -> Option<u16> {
        let rtcp_socket = self.rtcp_socket.as_ref()?;
        Some(rtcp_socket.socket.local_addr().unwrap().port())
    }
}

impl RtpTransport for DirectDtlsSrtpTransport {
    type Event = WhichTransport;

    async fn poll_event(&mut self) -> io::Result<Self::Event> {
        if let Some(rtcp_socket) = &mut self.rtcp_socket {
            loop {
                self.rtp_socket.recv_buf.resize(RECV_BUFFER_SIZE, 0);
                rtcp_socket.recv_buf.resize(RECV_BUFFER_SIZE, 0);

                select! {
                    res = self.rtp_socket.recv() => {
                        res?;
                        let Some(remote) = try_recv(&self.rtp_socket.socket, buf).await? else {
                            continue;
                        };
                        return Ok(( WhichTransport::Rtp));
                    }
                    res = rtcp_socket.socket.recv() => {
                        res?;
                        let Some(remote) = try_recv(&rtcp_socket.socket, buf).await? else {
                            continue;
                        };
                        return Ok((WhichTransport::Rtcp));
                    }
                    _ = self.handshake_interval.tick() => {
                        self.rtp_socket.do_handshake().await?;
                        rtcp_socket.do_handshake().await?;
                        continue;
                    }
                }
            }
        }

        loop {
            buf.resize(RECV_BUFFER_SIZE, 0);

            select! {
                res = self.rtp_socket.socket.recv_from(buf) => {
                    let (len, remote) = res?;
                    buf.truncate(len);
                    return Ok((remote, WhichTransport::Rtp));
                }
                _ = self.handshake_interval.tick() => {
                    self.rtp_socket.do_handshake().await?;
                    continue;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: Self::Event) -> io::Result<Option<&[u8]>> {
        let socket = match event {
            WhichTransport::Rtp => &mut self.rtp_socket,
            WhichTransport::Rtcp => self.rtcp_socket.as_mut().unwrap(),
        };

        socket.handle_packet(buf).await
    }

    async fn send_rtp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let Some((_, outbound)) = &mut self.rtp_socket.srtp else {
            return Err(io::Error::other("dtls-srtp not ready"));
        };

        outbound.protect(buf).map_err(io::Error::other)?;

        self.rtp_socket
            .socket
            .send_to(buf, self.rtp_socket.target)
            .await?;

        Ok(())
    }

    async fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let socket = self.rtcp_socket.as_mut().unwrap_or(&mut self.rtp_socket);

        let Some((_, outbound)) = &mut socket.srtp else {
            return Err(io::Error::other("dtls-srtp not ready"));
        };

        outbound.protect_rtcp(buf).map_err(io::Error::other)?;

        let socket = self.rtcp_socket.as_ref().unwrap_or(&self.rtp_socket);

        socket.socket.send_to(buf, socket.target).await?;

        Ok(())
    }

    fn is_ready(&self) -> bool {
        let rtp_is_ready = self.rtp_socket.srtp.is_some();
        let rtcp_is_ready = self
            .rtcp_socket
            .as_ref()
            .map(|s| s.srtp.is_some())
            .unwrap_or(true);

        rtp_is_ready && rtcp_is_ready
    }
}

async fn try_recv(socket: &UdpSocket, buf: &mut Vec<u8>) -> io::Result<Option<SocketAddr>> {
    let (len, remote) = match socket.try_recv_from(buf) {
        Ok((len, remote)) => (len, remote),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e),
    };

    buf.truncate(len);

    Ok(Some(remote))
}

impl Socket {
    async fn do_handshake(&mut self) -> io::Result<()> {
        println!("do handshake");

        if let Some((inbound, outbound)) = self.dtls.handshake()? {
            println!("ye");
            self.srtp = Some((inbound.into_session(), outbound.into_session()));
        }

        while let Some(to_send) = self.dtls.pop_to_send() {
            println!("I send {} to {}", to_send.len(), self.target);
            self.socket.send_to(&to_send, self.target).await?;
        }

        Ok(())
    }

    async fn handle_packet(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        match PacketKind::identify(buf) {
            PacketKind::Rtp => {
                let Some((inbound, ..)) = &mut self.srtp else {
                    return Ok(false);
                };

                inbound.unprotect(buf).map_err(io::Error::other)?;

                Ok(true)
            }
            PacketKind::Rtcp => {
                let Some((inbound, ..)) = &mut self.srtp else {
                    return Ok(false);
                };

                inbound.unprotect_rtcp(buf).map_err(io::Error::other)?;

                Ok(true)
            }
            PacketKind::Stun => Ok(false),
            PacketKind::Dtls => {
                println!("RECEIVE DTLS");
                self.dtls.receive(buf.clone());

                Ok(false)
            }
            PacketKind::Unknown => Ok(false),
        }
    }
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
