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
use sdp_types::{Fingerprint, FingerprintAlgorithm};
use srtp::openssl::Config;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
};
use tokio_openssl::SslStream;

pub(crate) struct DtlsSrtpAcceptor {
    stream: Pin<Box<SslStream<UdpAsyncRW>>>,
}

impl DtlsSrtpAcceptor {
    pub(crate) fn new(socket: Arc<UdpSocket>, remote_addr: SocketAddr) -> io::Result<Self> {
        let (cert, pkey) = mk_ca_cert().unwrap();

        let mut ctx = SslAcceptor::mozilla_modern(SslMethod::dtls())?;
        ctx.set_tlsext_use_srtp(srtp::openssl::SRTP_PROFILE_NAMES)?;
        ctx.set_private_key(&pkey)?;
        ctx.set_certificate(&cert)?;
        ctx.check_private_key()?;
        let ctx = ctx.build().into_context();

        let mut ssl = Ssl::new(&ctx)?;
        ssl.set_mtu(1200)?;

        Ok(Self {
            stream: Box::pin(SslStream::new(
                ssl,
                UdpAsyncRW {
                    socket,
                    target: remote_addr,
                },
            )?),
        })
    }

    pub(crate) fn fingerprint(&self) -> Fingerprint {
        let fingerprint = self
            .stream
            .ssl()
            .certificate()
            .unwrap()
            .digest(MessageDigest::sha256())
            .unwrap()
            .to_vec();

        Fingerprint {
            algorithm: FingerprintAlgorithm::SHA256,
            fingerprint,
        }
    }

    pub async fn accept(
        &mut self,
    ) -> io::Result<(
        srtp::openssl::InboundSession,
        srtp::openssl::OutboundSession,
    )> {
        self.stream
            .as_mut()
            .accept()
            .await
            .map_err(io::Error::other)?;

        let (inbound, outbound) =
            srtp::openssl::session_pair(self.stream.ssl(), Config::default()).unwrap();

        Ok((inbound, outbound))
    }
}

pub(crate) struct DtlsSrtpConnector {
    stream: Pin<Box<SslStream<UdpAsyncRW>>>,
    fingerprints: Vec<(MessageDigest, Vec<u8>)>,
}

impl DtlsSrtpConnector {
    pub(crate) fn new(
        socket: Arc<UdpSocket>,
        remote_addr: SocketAddr,
        fingerprints: Vec<(MessageDigest, Vec<u8>)>,
    ) -> io::Result<Self> {
        let (cert, pkey) = mk_ca_cert().unwrap();

        let mut ctx = SslAcceptor::mozilla_modern(SslMethod::dtls())?;
        ctx.set_tlsext_use_srtp(srtp::openssl::SRTP_PROFILE_NAMES)?;
        ctx.set_private_key(&pkey)?;
        ctx.set_certificate(&cert)?;
        ctx.check_private_key()?;
        let ctx = ctx.build().into_context();

        let mut ssl = Ssl::new(&ctx)?;
        ssl.set_mtu(1200)?;

        Ok(Self {
            stream: Box::pin(SslStream::new(
                ssl,
                UdpAsyncRW {
                    socket,
                    target: remote_addr,
                },
            )?),
            fingerprints,
        })
    }

    pub async fn connect(
        &mut self,
    ) -> io::Result<(
        srtp::openssl::InboundSession,
        srtp::openssl::OutboundSession,
    )> {
        self.stream
            .as_mut()
            .connect()
            .await
            .map_err(io::Error::other)?;

        let peer_cert = self.stream.ssl().peer_certificate().unwrap();

        for (digest, fingerprint) in &self.fingerprints {
            let peer_fingerprint = peer_cert.digest(*digest).unwrap();

            if peer_fingerprint.as_ref() != fingerprint {
                return Err(io::Error::other(
                    "fingerprint mismatch when establishing dtls connection",
                ));
            }
        }

        let (inbound, outbound) =
            srtp::openssl::session_pair(self.stream.ssl(), Config::default()).unwrap();

        Ok((inbound, outbound))
    }
}

#[derive(Debug)]
struct UdpAsyncRW {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
}

impl AsyncRead for UdpAsyncRW {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.socket.poll_recv_from(cx, buf).map_ok(|source| {
            // TODO: delete me?
            self.target = source;
        })
    }
}

impl AsyncWrite for UdpAsyncRW {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.socket.poll_send_to(cx, buf, self.target)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Make a CA certificate and private key
fn mk_ca_cert() -> Result<(X509, PKey<Private>), ErrorStack> {
    openssl::init();

    let rsa = Rsa::generate(2048)?;
    let key_pair = PKey::from_rsa(rsa)?;

    let mut x509_name = X509NameBuilder::new()?;
    x509_name.append_entry_by_text("C", "XX")?;
    x509_name.append_entry_by_text("ST", "XX")?;
    x509_name.append_entry_by_text("O", "EZK")?;
    x509_name.append_entry_by_text("CN", "EZK-DTLS-SRTP")?;
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
