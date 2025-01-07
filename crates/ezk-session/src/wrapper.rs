use crate::{
    events::TransportChange, transport::SocketUse, Codecs, Event, LocalMediaId, MediaId, Options,
    SocketId,
};
use sdp_types::{Direction, SessionDescription};
use std::{
    borrow::Cow,
    collections::HashMap,
    future::{pending, poll_fn},
    io,
    mem::MaybeUninit,
    net::IpAddr,
    task::Poll,
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    select,
    time::{sleep_until, Instant},
};

pub struct AsyncSdpSession {
    inner: super::SdpSession,

    sockets: HashMap<SocketId, UdpSocket>,

    timeout: Option<Instant>,
}

impl AsyncSdpSession {
    pub fn new(address: IpAddr) -> Self {
        Self {
            inner: super::SdpSession::new(address, Options::default()),
            sockets: HashMap::new(),
            timeout: None,
        }
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
                    self.inner
                        .set_transport_ports(transport_id, socket.local_addr()?.port(), None);
                    self.sockets
                        .insert(SocketId(transport_id, SocketUse::Rtp), socket);
                }
                TransportChange::CreateSocketPair(transport_id) => {
                    println!("Create socket pair {transport_id:?}");

                    let rtp_socket = UdpSocket::bind("0.0.0.0:0").await?;
                    let rtcp_socket = UdpSocket::bind("0.0.0.0:0").await?;

                    self.inner.set_transport_ports(
                        transport_id,
                        rtp_socket.local_addr()?.port(),
                        Some(rtcp_socket.local_addr()?.port()),
                    );

                    self.sockets
                        .insert(SocketId(transport_id, SocketUse::Rtp), rtp_socket);
                    self.sockets
                        .insert(SocketId(transport_id, SocketUse::Rtcp), rtcp_socket);
                }
                TransportChange::Remove(transport_id) => {
                    println!("Remove {transport_id:?}");

                    self.sockets.remove(&SocketId(transport_id, SocketUse::Rtp));
                    self.sockets
                        .remove(&SocketId(transport_id, SocketUse::Rtcp));
                }
                TransportChange::RemoveRtcpSocket(transport_id) => {
                    println!("Remove rtcp socket of {transport_id:?}");

                    self.sockets
                        .remove(&SocketId(transport_id, SocketUse::Rtcp));
                }
            }
        }

        Ok(())
    }

    async fn handle_events(&mut self) -> Result<(), super::Error> {
        while let Some(event) = self.inner.pop_event() {
            match event {
                Event::SendData {
                    socket,
                    data,
                    target,
                } => {
                    self.sockets[&socket].send_to(&data, target).await?;
                }
                Event::ConnectionState { media_id, old, new } => {
                    println!("Connection state of {media_id:?} changed from {old:?} to {new:?}");
                }
                Event::ReceiveRTP { media_id, packet } => {
                    println!("Received RTP on {media_id:?}");
                }
            }
        }

        Ok(())
    }

    pub async fn run(&mut self) -> io::Result<()> {
        let mut buf = vec![MaybeUninit::uninit(); 65535];
        let mut buf = ReadBuf::uninit(&mut buf);

        loop {
            buf.set_filled(0);

            self.inner.poll();
            self.handle_events().await.unwrap();

            self.timeout = self.inner.timeout().map(|d| Instant::now() + d);

            select! {
                (socket_id, result) = recv_sockets(&self.sockets, &mut buf) => {
                    let source = result?;

                    self.inner
                        .receive(socket_id, &mut Cow::Borrowed(buf.filled()), source);

                    self.handle_events().await.unwrap();
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
        Some(instant) => sleep_until(instant).await,
        None => pending().await,
    }
}

async fn recv_sockets(
    sockets: &HashMap<SocketId, UdpSocket>,
    buf: &mut ReadBuf<'_>,
) -> (SocketId, Result<std::net::SocketAddr, io::Error>) {
    buf.set_filled(0);

    poll_fn(|cx| {
        for (socket_id, socket) in sockets {
            let Poll::Ready(result) = socket.poll_recv_from(cx, buf) else {
                continue;
            };

            return Poll::Ready((*socket_id, result));
        }

        Poll::Pending
    })
    .await
}
