use super::{dtls_srtp::DtlsSrtpSession, PacketKind, RtpTransport, RECV_BUFFER_SIZE};
use base64::{prelude::BASE64_STANDARD, Engine};
use openssl::hash::MessageDigest;
use rand::RngCore;
use sdp_types::{Fingerprint, FingerprintAlgorithm, SrtpCrypto, SrtpKeyingMaterial, SrtpSuite};
use srtp::CryptoPolicy;
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
use tokio::net::UdpSocket;

pub struct DirectSrtpTransport {
    rtp_socket: Socket,
    rtcp_socket: Option<Socket>,
}

struct Socket {
    socket: UdpSocket,
    dtls: DtlsSrtpSession,
    srtp: Option<(srtp::Session, srtp::Session)>,
    target: SocketAddr,
}


impl DirectSrtpTransport {
    pub async fn sdes_srtp(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
        remote_crypto: &[SrtpCrypto],
    ) -> io::Result<(Self, Vec<SrtpCrypto>)> {
        let rtp_socket = UdpSocket::bind("0.0.0.0:0").await?;

        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports

        let rtcp_socket = if remote_rtcp_address.is_some() {
            Some(UdpSocket::bind("0.0.0.0:0").await?)
        } else {
            None
        };

        // Find best suite to use
        use sdp_types::SrtpSuite::*;

        let choice1 = remote_crypto
            .iter()
            .find(|c| c.suite == AES_256_CM_HMAC_SHA1_80 && !c.keys.is_empty());
        let choice2 = remote_crypto
            .iter()
            .find(|c| c.suite == AES_256_CM_HMAC_SHA1_32 && !c.keys.is_empty());
        let choice3 = remote_crypto
            .iter()
            .find(|c| c.suite == AES_CM_128_HMAC_SHA1_80 && !c.keys.is_empty());
        let choice4 = remote_crypto
            .iter()
            .find(|c| c.suite == AES_CM_128_HMAC_SHA1_32 && !c.keys.is_empty());

        let crypto = choice1
            .or(choice2)
            .or(choice3)
            .or(choice4)
            .ok_or_else(|| io::Error::other("No compatible srtp suite found"))?;

        let recv_key = BASE64_STANDARD
            .decode(&crypto.keys[0].key_and_salt)
            .map_err(io::Error::other)?;

        let suite = srtp_suite_to_policy(&crypto.suite).unwrap();

        let mut send_key = vec![0u8; suite.key_len()];
        rand::thread_rng().fill_bytes(&mut send_key);

        let inbound = srtp::Session::with_inbound_template(srtp::StreamPolicy {
            rtp: suite,
            rtcp: suite,
            key: &recv_key,
            ..Default::default()
        })
        .unwrap();

        let outbound = srtp::Session::with_outbound_template(srtp::StreamPolicy {
            rtp: suite,
            rtcp: suite,
            key: &send_key,
            ..Default::default()
        })
        .unwrap();

        Ok((
            Self {
                rtp_socket: Socket {
                    socket: rtp_socket,
                    dtls: None,
                    srtp: Some((inbound, outbound)),
                    target: remote_rtp_address,
                },
                rtcp_socket: remote_rtcp_address.map(|remote_rtcp_address| Socket {
                    socket: rtcp_socket.unwrap(),
                    dtls: None,
                    srtp: None,
                    target: remote_rtcp_address,
                }),
            },
            vec![SrtpCrypto {
                tag: crypto.tag,
                suite: crypto.suite.clone(),
                keys: vec![SrtpKeyingMaterial {
                    key_and_salt: BASE64_STANDARD.encode(&send_key).into(),
                    lifetime: None,
                    mki: None,
                }],
                params: vec![],
            }],
        ))
    }

    pub async fn dtls_srtp(
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
            socket: rtp_socket,
            state: rtp_state,
            target: remote_rtp_address,
        };

        // Setup RTCP socket if required
        let rtcp_socket = if let Some(remote_rtcp_address) = remote_rtcp_address {
            let rtcp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            let rtcp_acceptor = DtlsSrtpSession::new(remote_fingerprints, setup)?;
            let rtcp_state = State::DtlsHandshaking(rtcp_acceptor);

            Some(Socket {
                socket: rtcp_socket,
                state: rtcp_state,
                target: remote_rtcp_address,
            })
        } else {
            None
        };

        Ok((
            Self {
                rtp_socket,
                rtcp_socket,
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

impl RtpTransport for DirectSrtpTransport {
    async fn recv(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        // if let Some(rtcp_socket) = &self.rtcp_socket {
        //     // Poll both rtp_socket & rtcp_socket for readyness and try_read once available
        //     loop {
        //         select! {
        //             result = self.rtp_socket.readable() => {
        //                 result?;
        //                 if try_recv(inbound, &self.rtp_socket, buf).await? {
        //                     return Ok(())
        //                 }
        //             },
        //             result = rtcp_socket.readable() => {
        //                 result?;
        //                 if try_recv(inbound, rtcp_socket, buf).await? {
        //                     return Ok(())
        //                 }
        //             },
        //         }
        //     }
        // }

        loop {
            if let State::DtlsHandshaking(rtp_acceptor) = &mut self.rtp_socket.state {
                rtp_acceptor.handshake()?;

                while let Some(to_send) = rtp_acceptor.pop_to_send() {
                    self.rtp_socket
                        .socket
                        .send_to(&to_send, self.rtp_socket.target)
                        .await?;
                }
            }

            buf.resize(RECV_BUFFER_SIZE, 0);
            let (len, remote) = self.rtp_socket.socket.recv_from(buf).await?;

            buf.truncate(len);

            match PacketKind::identify(buf) {
                PacketKind::Rtp => {
                    let State::SrtpEstablished { inbound, .. } = &mut self.rtp_socket.state else {
                        continue;
                    };
                    inbound.unprotect(buf).map_err(io::Error::other)?;
                    return Ok(());
                }
                PacketKind::Rtcp => {
                    let State::SrtpEstablished { inbound, .. } = &mut self.rtp_socket.state else {
                        continue;
                    };
                    inbound.unprotect_rtcp(buf).map_err(io::Error::other)?;
                    return Ok(());
                }
                PacketKind::Stun => {
                    // check_for_stun_binding_request(&self.rtp_socket, buf, remote).await?;
                }
                PacketKind::Dtls => match &mut self.rtp_socket.state {
                    State::DtlsHandshaking(dtls_srtp_acceptor) => {
                        if let Some((inbound, outbound)) =
                            dtls_srtp_acceptor.handshake(buf.clone())?
                        {
                            while let Some(to_send) = dtls_srtp_acceptor.pop_to_send() {
                                self.rtp_socket
                                    .socket
                                    .send_to(&to_send, self.rtp_socket.target)
                                    .await?;
                            }

                            self.rtp_socket.state = State::SrtpEstablished {
                                inbound: inbound.into_session(),
                                outbound: outbound.into_session(),
                            };
                            continue;
                        } else {
                            while let Some(to_send) = dtls_srtp_acceptor.pop_to_send() {
                                self.rtp_socket
                                    .socket
                                    .send_to(&to_send, self.rtp_socket.target)
                                    .await?;
                            }
                        }
                    }
                    State::SrtpEstablished { inbound, outbound } => todo!(),
                    State::UseRTPState => unreachable!(),
                },
                PacketKind::Unknown => {
                    continue;
                }
            }
        }
    }

    async fn send_rtp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let State::SrtpEstablished { outbound, .. } = &mut self.rtp_socket.state else {
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
        // let State::SrtpEstablished { outbound, .. } = &mut self.state else {
        //     return Err(io::Error::other("dtls-srtp not ready"));
        // };

        // outbound.protect_rtcp(buf).map_err(io::Error::other)?;

        // let socket = self.rtcp_socket.as_ref().unwrap_or(&self.rtp_socket);
        // let target = self.remote_rtcp_address.unwrap_or(self.remote_rtp_address);

        // socket.send_to(buf, target).await?;

        Ok(())
    }

    fn is_ready(&self) -> bool {
        // matches!(self.state, State::SrtpEstablished { .. })
        false
    }
}

async fn try_recv(
    inbound: &mut srtp::Session,
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
        PacketKind::Dtls => todo!(),
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

fn srtp_suite_to_policy(suite: &SrtpSuite) -> Option<CryptoPolicy> {
    match suite {
        SrtpSuite::AES_CM_128_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_128_hmac_sha1_80()),
        SrtpSuite::AES_CM_128_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_128_hmac_sha1_32()),
        SrtpSuite::AES_192_CM_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_192_hmac_sha1_80()),
        SrtpSuite::AES_192_CM_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_192_hmac_sha1_32()),
        SrtpSuite::AES_256_CM_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_256_hmac_sha1_80()),
        SrtpSuite::AES_256_CM_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_256_hmac_sha1_32()),
        SrtpSuite::AEAD_AES_128_GCM => Some(CryptoPolicy::aes_gcm_128_16_auth()),
        SrtpSuite::AEAD_AES_256_GCM => Some(CryptoPolicy::aes_gcm_256_16_auth()),
        _ => None,
    }
}
