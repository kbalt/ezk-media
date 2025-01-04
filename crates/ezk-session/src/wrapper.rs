use crate::{transport::SocketUse, Codecs, Event, LocalMediaId, MediaId, Options, SocketId};
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
        limit: usize,
        direction: Direction,
    ) -> LocalMediaId {
        self.inner.add_local_media(codecs, limit, direction)
    }

    pub fn add_media(&mut self, local_media_id: LocalMediaId, direction: Direction) -> MediaId {
        self.inner.add_media(local_media_id, direction)
    }

    pub async fn create_offer(&mut self) -> SessionDescription {
        self.create_pending_sockets().await.unwrap();
        self.inner.create_sdp_offer()
    }

    pub async fn receive_sdp_offer(
        &mut self,
        offer: SessionDescription,
    ) -> Result<SessionDescription, super::Error> {
        let state = self.inner.receive_sdp_offer(offer)?;

        self.create_pending_sockets().await?;

        Ok(self.inner.create_sdp_answer(state))
    }

    async fn create_pending_sockets(&mut self) -> Result<(), crate::Error> {
        for new_transport in self.inner.new_transports() {
            if new_transport.rtp_port.is_none() {
                let socket = UdpSocket::bind("0.0.0.0:0").await?;
                *new_transport.rtp_port = Some(socket.local_addr()?.port());
                self.sockets
                    .insert(SocketId(new_transport.id, SocketUse::Rtp), socket);
            }

            if !new_transport.rtcp_mux && new_transport.rtcp_port.is_none() {
                let socket = UdpSocket::bind("0.0.0.0:0").await?;
                *new_transport.rtcp_port = Some(socket.local_addr()?.port());
                self.sockets
                    .insert(SocketId(new_transport.id, SocketUse::Rtcp), socket);
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
