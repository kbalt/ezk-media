use crate::{
    events::TransportChange, Codecs, Event, LocalMediaId, MediaId, Options, ReceivedPkt, SocketId,
};
use ezk_ice::Component;
use sdp_types::{Direction, SessionDescription};
use socket::Socket;
use std::{
    collections::HashMap,
    future::{pending, poll_fn},
    io::{self},
    mem::MaybeUninit,
    net::{IpAddr, SocketAddr},
    task::Poll,
    time::Instant,
};
use tokio::{io::ReadBuf, net::UdpSocket, select, time::sleep_until};

mod socket;

pub struct AsyncSdpSession {
    inner: super::SdpSession,
    sockets: HashMap<SocketId, Socket>,
    timeout: Option<Instant>,
    ips: Vec<IpAddr>,
}

impl AsyncSdpSession {
    pub fn new(address: IpAddr) -> Self {
        Self {
            inner: super::SdpSession::new(
                address,
                Options {
                    offer_ice: true,
                    ..Options::default()
                },
            ),
            sockets: HashMap::new(),
            timeout: None,
            ips: local_ip_address::linux::list_afinet_netifas()
                .unwrap()
                .into_iter()
                .map(|(_, addr)| addr)
                .collect(),
        }
    }

    pub fn add_stun_server(&mut self, server: SocketAddr) {
        self.inner.add_stun_server(server);
    }

    /// Register codecs for a media type with a limit of how many media session by can be created
    pub fn add_local_media(
        &mut self,
        codecs: Codecs,
        limit: u32,
        direction: Direction,
    ) -> LocalMediaId {
        self.inner.add_local_media(codecs, limit, direction)
    }

    pub fn add_media(&mut self, local_media_id: LocalMediaId, direction: Direction) -> MediaId {
        self.inner.add_media(local_media_id, direction)
    }

    pub async fn create_offer(&mut self) -> SessionDescription {
        self.handle_transport_changes().await.unwrap();
        self.inner.create_sdp_offer()
    }

    pub async fn receive_sdp_offer(
        &mut self,
        offer: SessionDescription,
    ) -> Result<SessionDescription, super::Error> {
        let state = self.inner.receive_sdp_offer(offer)?;

        self.handle_transport_changes().await?;

        Ok(self.inner.create_sdp_answer(state))
    }

    pub async fn receive_sdp_answer(
        &mut self,
        answer: SessionDescription,
    ) -> Result<(), super::Error> {
        self.inner.receive_sdp_answer(answer);

        self.handle_transport_changes().await?;

        Ok(())
    }

    async fn handle_transport_changes(&mut self) -> Result<(), crate::Error> {
        for change in self.inner.transport_changes() {
            match change {
                TransportChange::CreateSocket(transport_id) => {
                    println!("Create socket {transport_id:?}");
                    let socket = UdpSocket::bind("0.0.0.0:0").await?;

                    self.inner.set_transport_ports(
                        transport_id,
                        &self.ips,
                        socket.local_addr()?.port(),
                        None,
                    );

                    self.sockets
                        .insert(SocketId(transport_id, Component::Rtp), Socket::new(socket));
                }
                TransportChange::CreateSocketPair(transport_id) => {
                    println!("Create socket pair {transport_id:?}");

                    let rtp_socket = UdpSocket::bind("0.0.0.0:0").await?;
                    let rtcp_socket = UdpSocket::bind("0.0.0.0:0").await?;

                    self.inner.set_transport_ports(
                        transport_id,
                        &self.ips,
                        rtp_socket.local_addr()?.port(),
                        Some(rtcp_socket.local_addr()?.port()),
                    );

                    self.sockets.insert(
                        SocketId(transport_id, Component::Rtp),
                        Socket::new(rtp_socket),
                    );
                    self.sockets.insert(
                        SocketId(transport_id, Component::Rtcp),
                        Socket::new(rtcp_socket),
                    );
                }
                TransportChange::Remove(transport_id) => {
                    println!("Remove {transport_id:?}");

                    self.sockets.remove(&SocketId(transport_id, Component::Rtp));
                    self.sockets
                        .remove(&SocketId(transport_id, Component::Rtcp));
                }
                TransportChange::RemoveRtcpSocket(transport_id) => {
                    println!("Remove rtcp socket of {transport_id:?}");

                    self.sockets
                        .remove(&SocketId(transport_id, Component::Rtcp));
                }
            }
        }

        Ok(())
    }

    fn handle_events(&mut self) -> Result<(), super::Error> {
        while let Some(event) = self.inner.pop_event() {
            match event {
                Event::SendData {
                    socket,
                    data,
                    source,
                    target,
                } => {
                    if let Some(socket) = self.sockets.get_mut(&socket) {
                        socket.enqueue(data, source, target);
                    } else {
                        println!("invalid socket id")
                    }
                }
                Event::ReceiveRTP { media_id, packet } => {
                    println!("Received RTP on {media_id:?}");
                }
                _ => todo!(),
            }
        }

        Ok(())
    }

    pub async fn run(&mut self) -> io::Result<()> {
        let mut buf = vec![MaybeUninit::uninit(); 65535];
        let mut buf = ReadBuf::uninit(&mut buf);

        loop {
            self.inner.poll(Instant::now());

            self.handle_events().unwrap();
            self.timeout = self.inner.timeout().map(|d| Instant::now() + d);

            select! {
                (socket_id, result) = poll_sockets(&mut self.sockets, &mut buf) => {
                    let (dst, source) = result?;

                    let pkt = ReceivedPkt {
                        data: buf.filled().to_vec(),
                        source,
                        destination: dst,
                        component: socket_id.1
                    };

                    self.inner.receive(socket_id.0, pkt);
                    self.handle_events().unwrap();

                    buf.set_filled(0);
                }
                _ = timeout(self.timeout) => {
                    continue;
                }
            }
        }
    }
}

async fn timeout(instant: Option<Instant>) {
    match instant {
        Some(instant) => sleep_until(instant.into()).await,
        None => pending().await,
    }
}

async fn poll_sockets(
    sockets: &mut HashMap<SocketId, Socket>,
    buf: &mut ReadBuf<'_>,
) -> (SocketId, Result<(SocketAddr, SocketAddr), io::Error>) {
    buf.set_filled(0);

    poll_fn(|cx| {
        for (socket_id, socket) in sockets.iter_mut() {
            socket.send_pending(cx);

            if let Poll::Ready(result) = socket.poll_recv_from(cx, buf) {
                return Poll::Ready((*socket_id, result));
            }
        }

        Poll::Pending
    })
    .await
}
