use std::{net::SocketAddr, sync::Arc};

use tokio::net::UdpSocket;

pub enum MediaTransport {
    Direct(DirectRtpTransport),
}

pub struct DirectRtpTransport {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,
}

impl DirectRtpTransport {
    pub fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: SocketAddr,
        rtcp_mux: bool,
    ) -> Self {
        Self {}
    }
}
