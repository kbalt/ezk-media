use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    error::ErrorStack,
    hash::MessageDigest,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{Ssl, SslAcceptor, SslMethod},
    x509::{
        extension::{BasicConstraints, KeyUsage, SubjectKeyIdentifier},
        X509NameBuilder, X509,
    },
};
use srtp::openssl::Config;
use std::{
    io::{self},
    net::SocketAddr,
    sync::Arc,
};
use stun_types::{
    attributes::XorMappedAddress,
    builder::MessageBuilder,
    header::{Class, Method},
    is_stun_message,
    parse::ParsedMessage,
    IsStunMessageInfo,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
    select,
};
use tokio_openssl::SslStream;

use super::RtpTransport;

/// Make a CA certificate and private key
fn mk_ca_cert() -> Result<(X509, PKey<Private>), ErrorStack> {
    let rsa = Rsa::generate(2048)?;
    let key_pair = PKey::from_rsa(rsa)?;

    let mut x509_name = X509NameBuilder::new()?;
    x509_name.append_entry_by_text("C", "US")?;
    x509_name.append_entry_by_text("ST", "TX")?;
    x509_name.append_entry_by_text("O", "Some CA organization")?;
    x509_name.append_entry_by_text("CN", "ca test")?;
    let x509_name = x509_name.build();

    let mut cert_builder = X509::builder()?;
    cert_builder.set_version(2)?;
    let serial_number = {
        let mut serial = BigNum::new()?;
        serial.rand(159, MsbOption::MAYBE_ZERO, false)?;
        serial.to_asn1_integer()?
    };
    cert_builder.set_serial_number(&serial_number)?;
    cert_builder.set_subject_name(&x509_name)?;
    cert_builder.set_issuer_name(&x509_name)?;
    cert_builder.set_pubkey(&key_pair)?;
    let not_before = Asn1Time::days_from_now(0)?;
    cert_builder.set_not_before(&not_before)?;
    let not_after = Asn1Time::days_from_now(365)?;
    cert_builder.set_not_after(&not_after)?;

    cert_builder.append_extension(BasicConstraints::new().critical().ca().build()?)?;
    cert_builder.append_extension(
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
    )?;

    let subject_key_identifier =
        SubjectKeyIdentifier::new().build(&cert_builder.x509v3_context(None, None))?;
    cert_builder.append_extension(subject_key_identifier)?;

    cert_builder.sign(&key_pair, MessageDigest::sha256())?;
    let cert = cert_builder.build();

    Ok((cert, key_pair))
}

#[derive(Debug)]
struct AcceptAsyncRW {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
}

impl AsyncRead for AcceptAsyncRW {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.socket.poll_recv(cx, buf)
    }
}

impl AsyncWrite for AcceptAsyncRW {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        self.socket.poll_send_to(cx, buf, self.target)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

pub struct LibSrtpTransport {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,
}

impl LibSrtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
    ) -> io::Result<Self> {
        let rtp_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

        openssl::init();

        let (cert, pkey) = mk_ca_cert().unwrap();

        let mut ctx = SslAcceptor::mozilla_modern(SslMethod::dtls())?;
        ctx.set_tlsext_use_srtp(srtp::openssl::SRTP_PROFILE_NAMES)?;
        ctx.set_private_key(&pkey)?;
        ctx.set_certificate(&cert)?;
        ctx.check_private_key()?;
        let ctx = ctx.build().into_context();

        let mut ssl = Ssl::new(&ctx)?;
        ssl.set_mtu(1200)?;

        let mut stream = Box::pin(
            SslStream::new(
                ssl,
                AcceptAsyncRW {
                    socket: rtp_socket.clone(),
                    target: remote_rtp_address,
                },
            )
            .unwrap(),
        );

        stream.as_mut().accept().await.unwrap();

        let (inbound, outbound) =
            srtp::openssl::session_pair(stream.ssl(), Config::default()).unwrap();

        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports

        let rtcp_socket = if remote_rtcp_address.is_some() {
            Some(Arc::new(UdpSocket::bind("0.0.0.0:0").await?))
        } else {
            None
        };

        Ok(Self {
            rtp_socket,
            rtcp_socket,
            remote_rtp_address,
            remote_rtcp_address,
        })
    }

    pub fn local_rtp_port(&self) -> u16 {
        self.rtp_socket.local_addr().unwrap().port()
    }

    pub fn local_rtcp_port(&self) -> Option<u16> {
        let rtcp_socket = self.rtcp_socket.as_ref()?;

        Some(rtcp_socket.local_addr().unwrap().port())
    }
}

impl RtpTransport for LibSrtpTransport {
    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(rtcp_socket) = &self.rtcp_socket {
            // Poll both rtp_socket & rtcp_socket for readyness and try_read once available
            loop {
                let result = select! {
                    result = self.rtp_socket.readable() => {
                        result?;
                        try_recv(&self.rtp_socket, buf).await
                    },
                    result = rtcp_socket.readable() => {
                        result?;
                        try_recv(rtcp_socket, buf).await
                    },
                };

                if let Some(len) = result? {
                    return Ok(len);
                }
            }
        }

        loop {
            // No rtcp_socket, just read from the rtp_socket
            let (len, remote) = self.rtp_socket.recv_from(buf).await?;

            if !check_for_stun_binding_request(&self.rtp_socket, buf, remote).await? {
                return Ok(len);
            }
        }
    }

    async fn send_rtp(&mut self, buf: &[u8]) -> io::Result<()> {
        self.rtp_socket
            .send_to(buf, self.remote_rtp_address)
            .await?;

        Ok(())
    }

    async fn send_rtcp(&mut self, buf: &[u8]) -> io::Result<()> {
        if let Some(rtcp_socket) = &self.rtcp_socket {
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
}

async fn try_recv(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<Option<usize>> {
    let (len, remote) = match socket.try_recv_from(buf) {
        Ok((len, remote)) => (len, remote),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e),
    };

    if check_for_stun_binding_request(socket, buf, remote).await? {
        Ok(None)
    } else {
        Ok(Some(len))
    }
}

async fn check_for_stun_binding_request(
    socket: &UdpSocket,
    buf: &[u8],
    remote: SocketAddr,
) -> io::Result<bool> {
    let len = if let IsStunMessageInfo::Yes { len } = is_stun_message(buf) {
        len
    } else {
        return Ok(false);
    };

    let Ok(e) = ParsedMessage::parse(buf[..len].to_vec()) else {
        return Ok(false);
    };

    if e.class == Class::Request && e.method == Method::Binding {
        let mut msg = MessageBuilder::new(Class::Success, Method::Binding, e.tsx_id);
        msg.add_attr(&XorMappedAddress(remote)).unwrap();
        let msg = msg.finish();
        socket.send_to(&msg, remote).await?;
        return Ok(true);
    }

    Ok(false)
}
