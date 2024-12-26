use super::{RtpTransport, WhichTransport, RECV_BUFFER_SIZE};
use std::{io, net::SocketAddr};
use tokio::{net::UdpSocket, select};

pub struct DirectRtpTransport {
    rtp_socket: Socket,
    rtcp_socket: Option<Socket>,
}

struct Socket {
    socket: UdpSocket,
    recv_buf: Vec<u8>,
    target: SocketAddr,
}

impl Socket {
    async fn recv(&mut self) -> io::Result<SocketAddr> {
        let (len, source) = self.socket.recv_from(&mut self.recv_buf).await?;
        self.recv_buf.truncate(len);
        Ok(source)
    }
}

impl DirectRtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
    ) -> io::Result<Self> {
        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports

        let rtp_socket = Socket {
            socket: UdpSocket::bind("0.0.0.0:0").await?,
            recv_buf: vec![0u8; RECV_BUFFER_SIZE],
            target: remote_rtp_address,
        };

        let rtcp_socket = if let Some(remote_rtcp_address) = remote_rtcp_address {
            Some(Socket {
                socket: UdpSocket::bind("0.0.0.0:0").await?,
                recv_buf: vec![0u8; RECV_BUFFER_SIZE],
                target: remote_rtcp_address,
            })
        } else {
            None
        };

        Ok(Self {
            rtp_socket,
            rtcp_socket,
        })
    }

    pub fn local_rtp_port(&self) -> u16 {
        self.rtp_socket.socket.local_addr().unwrap().port()
    }

    pub fn local_rtcp_port(&self) -> Option<u16> {
        let rtcp_socket = self.rtcp_socket.as_ref()?;

        Some(rtcp_socket.socket.local_addr().unwrap().port())
    }
}

impl RtpTransport for DirectRtpTransport {
    type Event = WhichTransport;

    async fn poll_event(&mut self) -> io::Result<Self::Event> {
        if let Some(rtcp_socket) = &mut self.rtcp_socket {
            // Poll both rtp_socket & rtcp_socket for readyness and try_read once available
            select! {
                result = self.rtp_socket.recv() => {
                    result?;
                    return Ok(WhichTransport::Rtp);
                },
                result = rtcp_socket.recv() => {
                    result?;
                    return Ok(WhichTransport::Rtcp);
                },
            }
        }

        // No rtcp_socket, just read from the rtp_socket
        self.rtp_socket.recv().await?;

        Ok(WhichTransport::Rtp)
    }

    async fn handle_event(&mut self, event: Self::Event) -> io::Result<Option<&[u8]>> {
        let socket = match event {
            WhichTransport::Rtp => &mut self.rtp_socket,
            WhichTransport::Rtcp => self.rtcp_socket.as_mut().unwrap(),
        };

        Ok(Some(&socket.recv_buf[..]))
    }

    async fn send_rtp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        self.rtp_socket
            .socket
            .send_to(buf, self.rtp_socket.target)
            .await?;

        Ok(())
    }

    async fn send_rtcp(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        if let Some(rtcp_socket) = &self.rtcp_socket {
            rtcp_socket.socket.send_to(buf, rtcp_socket.target).await?;

            Ok(())
        } else {
            self.send_rtp(buf).await
        }
    }

    fn is_ready(&self) -> bool {
        true
    }
}

fn try_recv(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
    match socket.try_recv_from(buf) {
        Ok((len, remote)) => Ok(Some((len, remote))),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}
