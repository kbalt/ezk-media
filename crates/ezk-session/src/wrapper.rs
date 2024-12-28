use crate::{Codecs, Instruction, LocalMediaId, SocketId};
use sdp_types::{Direction, SessionDescription};
use std::{
    borrow::Cow, collections::HashMap, future::poll_fn, io, mem::MaybeUninit, net::IpAddr,
    task::Poll,
};
use tokio::{io::ReadBuf, net::UdpSocket};

pub struct AsyncSdpSession {
    inner: super::SdpSession,

    sockets: HashMap<SocketId, UdpSocket>,
}

impl AsyncSdpSession {
    pub fn new(address: IpAddr) -> Self {
        Self {
            inner: super::SdpSession::new(address),
            sockets: HashMap::new(),
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

    pub fn remove_local_media(&mut self, local_media_id: LocalMediaId) {
        self.inner.remove_local_media(local_media_id);
    }

    pub async fn receive_sdp_offer(
        &mut self,
        offer: SessionDescription,
    ) -> Result<(), super::Error> {
        self.inner.receive_sdp_offer(offer)?;

        self.handle_instructions().await
    }

    pub fn create_sdp_answer(&self) -> SessionDescription {
        self.inner.create_sdp_answer()
    }

    async fn handle_instructions(&mut self) -> Result<(), super::Error> {
        while let Some(ins) = self.inner.pop_instruction() {
            match ins {
                Instruction::CreateUdpSocketPair { socket_ids } => {
                    let socket1 = UdpSocket::bind("0.0.0.0:0").await?;
                    let socket2 = UdpSocket::bind("0.0.0.0:0").await?;

                    self.inner
                        .set_socket_port(socket_ids[0], socket1.local_addr()?.port());
                    self.inner
                        .set_socket_port(socket_ids[1], socket2.local_addr()?.port());

                    self.sockets.insert(socket_ids[0], socket1);
                    self.sockets.insert(socket_ids[1], socket2);
                }
                Instruction::CreateUdpSocket { socket_id } => {
                    let socket = UdpSocket::bind("0.0.0.0:0").await?;
                    self.inner
                        .set_socket_port(socket_id, socket.local_addr()?.port());
                    self.sockets.insert(socket_id, socket);
                }
                Instruction::SendData {
                    socket,
                    data,
                    target,
                } => {
                    self.sockets[&socket].send_to(&data, target).await?;
                }
                Instruction::ReceiveRTP { packet } => {}
                Instruction::TrackAdded {} => {}
            }
        }

        Ok(())
    }

    pub async fn run(&mut self) -> io::Result<()> {
        let mut buf = vec![MaybeUninit::uninit(); 65535];
        let mut buf = ReadBuf::uninit(&mut buf);

        loop {
            buf.set_filled(0);

            let (socket_id, result) = poll_fn(|cx| {
                for (socket_id, socket) in &self.sockets {
                    let Poll::Ready(result) = socket.poll_recv_from(cx, &mut buf) else {
                        continue;
                    };

                    return Poll::Ready((socket_id, result));
                }

                Poll::Pending
            })
            .await;

            let source = result?;

            self.inner
                .receive(*socket_id, &mut Cow::Borrowed(buf.filled()), source);

            self.handle_instructions().await.unwrap();
        }
    }
}
