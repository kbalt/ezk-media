use crate::{
    events::TransportChange, transport::SocketUse, Codecs, Event, LocalMediaId, MediaId, Options,
    SocketId,
};
use sdp_types::{Direction, SessionDescription};
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    future::{pending, poll_fn},
    io,
    mem::MaybeUninit,
    net::{IpAddr, SocketAddr},
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
    sockets: HashMap<SocketId, Socket>,
    timeout: Option<Instant>,
}

struct Socket {
    socket: UdpSocket,
    to_send: VecDeque<(Vec<u8>, SocketAddr)>,
}

impl Socket {
    fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            to_send: VecDeque::new(),
        }
    }
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

                    self.sockets.insert(
                        SocketId(transport_id, SocketUse::Rtp),
                        Socket {
                            socket,
                            to_send: VecDeque::new(),
                        },
                    );
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

                    self.sockets.insert(
                        SocketId(transport_id, SocketUse::Rtp),
                        Socket::new(rtp_socket),
                    );
                    self.sockets.insert(
                        SocketId(transport_id, SocketUse::Rtcp),
                        Socket::new(rtcp_socket),
                    );
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

    fn handle_events(&mut self) -> Result<(), super::Error> {
        while let Some(event) = self.inner.pop_event() {
            match event {
                Event::SendData {
                    socket,
                    data,
                    target,
                } => {
                    self.sockets
                        .get_mut(&socket)
                        .unwrap()
                        .to_send
                        .push_back((data, target));
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
            self.handle_events().unwrap();

            self.timeout = self.inner.timeout().map(|d| Instant::now() + d);

            select! {
                (socket_id, result) = poll_sockets(&mut self.sockets, &mut buf) => {
                    let source = result?;

                    self.inner
                        .receive(socket_id, &mut Cow::Borrowed(buf.filled()), source);

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
        Some(instant) => sleep_until(instant).await,
        None => pending().await,
    }
}

async fn poll_sockets(
    sockets: &mut HashMap<SocketId, Socket>,
    buf: &mut ReadBuf<'_>,
) -> (SocketId, Result<std::net::SocketAddr, io::Error>) {
    poll_fn(|cx| {
        for (socket_id, socket) in sockets.iter_mut() {
            println!("{:?}", socket.socket.local_addr());

            while let Some((data, target)) = socket.to_send.front() {
                match socket.socket.poll_send_to(cx, data, *target) {
                    Poll::Ready(Ok(..)) => {
                        socket.to_send.pop_front();
                    }
                    Poll::Ready(Err(..)) => {
                        todo!()
                    }
                    Poll::Pending => continue,
                }
            }

            let Poll::Ready(result) = socket.socket.poll_recv_from(cx, buf) else {
                continue;
            };

            return Poll::Ready((*socket_id, result));
        }

        Poll::Pending
    })
    .await
}
