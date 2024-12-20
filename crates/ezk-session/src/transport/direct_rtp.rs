use super::RtpTransport;
use std::{io, net::SocketAddr, sync::Arc};
use stun_types::{
    attributes::XorMappedAddress,
    builder::MessageBuilder,
    header::{Class, Method},
    is_stun_message,
    parse::ParsedMessage,
    IsStunMessageInfo,
};
use tokio::{net::UdpSocket, select};

pub struct DirectRtpTransport {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,
}

impl DirectRtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
    ) -> io::Result<Self> {
        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports
        let rtp_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

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

impl RtpTransport for DirectRtpTransport {
    async fn recv(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
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
                    buf.truncate(len);
                    return Ok(());
                }
            }
        }

        loop {
            // No rtcp_socket, just read from the rtp_socket
            let (len, remote) = self.rtp_socket.recv_from(buf).await?;

            if !check_for_stun_binding_request(&self.rtp_socket, buf, remote).await? {
                buf.truncate(len);
                return Ok(());
            }
        }
    }

    async fn send_rtp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        self.rtp_socket
            .send_to(buf, self.remote_rtp_address)
            .await?;

        Ok(())
    }

    async fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
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

    fn is_ready(&self) -> bool {
        true
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
