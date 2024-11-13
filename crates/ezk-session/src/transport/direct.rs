use std::{io, net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, select};

use super::RtpTransport;

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

    fn try_recv(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match socket.try_recv(buf) {
            Ok(len) => Ok(Some(len)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl RtpTransport for DirectRtpTransport {
    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = if let Some(rtcp_socket) = &self.rtcp_socket {
                select! {
                    result = self.rtp_socket.readable() => {
                        result?;
                        Self::try_recv(&self.rtp_socket, buf)
                    },
                    result = rtcp_socket.readable() => {
                        result?;
                        Self::try_recv(rtcp_socket, buf)
                    },
                }
            } else {
                self.rtp_socket.readable().await?;

                Self::try_recv(&self.rtp_socket, buf)
            };

            if let Some(len) = result? {
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
