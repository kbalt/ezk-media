use super::RtpMpscSource;
use ezk::{BoxedSourceCancelSafe, Source, SourceEvent};
use ezk_rtp::rtcp_types::Compound;
use ezk_rtp::{Rtp, RtpConfigRange, RtpPacket, RtpSession};
use std::future::pending;
use std::num::NonZero;
use std::time::Duration;
use std::{io, net::SocketAddr, sync::Arc};
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, interval_at, Instant};

const RECV_BUFFER_SIZE: usize = 65535;

pub(crate) struct DirectRtpTransport {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: Option<SocketAddr>,

    command_tx: mpsc::Sender<ToTaskCommand>,
}

impl DirectRtpTransport {
    pub async fn new(
        remote_rtp_address: SocketAddr,
        remote_rtcp_address: Option<SocketAddr>,
        clock_rate: u32,
    ) -> Result<Self, io::Error> {
        // TODO: choose ports from a port range, and ideally have rtp and rtcp have adjacent ports
        let rtp_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

        let rtcp_socket = if remote_rtcp_address.is_some() {
            Some(Arc::new(UdpSocket::bind("0.0.0.0:0").await?))
        } else {
            None
        };

        let (command_tx, command_rx) = mpsc::channel(1);

        tokio::spawn(
            Task {
                state: TaskState::Ok,
                session: RtpSession::new(rand::random(), clock_rate),
                rtp_socket: rtp_socket.clone(),
                rtcp_socket: rtcp_socket.clone(),
                remote_rtp_address,
                remote_rtcp_address: remote_rtcp_address.unwrap_or(remote_rtp_address),
                encode_buf: vec![],
                rtp_sender_source: None,
                rtp_receiver_sink: None,
                command_rx,
            }
            .run(),
        );

        Ok(Self {
            rtp_socket,
            rtcp_socket,
            remote_rtp_address,
            remote_rtcp_address,
            command_tx,
        })
    }

    pub fn local_rtp_port(&self) -> u16 {
        self.rtp_socket.local_addr().unwrap().port()
    }

    pub fn local_rtcp_port(&self) -> Option<u16> {
        let rtcp_socket = self.rtcp_socket.as_ref()?;

        Some(rtcp_socket.local_addr().unwrap().port())
    }

    pub fn remote_rtp_address(&self) -> SocketAddr {
        self.remote_rtp_address
    }

    pub async fn set_sender(&mut self, mut source: BoxedSourceCancelSafe<Rtp>, media_pt: u8) {
        source
            .negotiate_config(vec![RtpConfigRange {
                pt: media_pt.into(),
            }])
            .await
            .unwrap();

        self.command_tx
            .send(ToTaskCommand::SetSender(source))
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn remove_sender(&mut self) {
        self.command_tx
            .send(ToTaskCommand::RemoveSender)
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn set_receiver(&mut self, media_pt: u8) -> BoxedSourceCancelSafe<Rtp> {
        let (tx, rx) = mpsc::channel(8);

        // TODO: this is giga cringe with dtmf, which has a different pt
        let source = RtpMpscSource { rx, pt: media_pt };

        self.command_tx
            .send(ToTaskCommand::SetReceiver(tx))
            .await
            .expect("task must not exit while command_tx exists");

        source.boxed_cancel_safe()
    }

    pub async fn remove_receiver(&mut self) {
        self.command_tx
            .send(ToTaskCommand::RemoveReceiver)
            .await
            .expect("task must not exit while command_tx exists");
    }
}

enum ToTaskCommand {
    SetSender(BoxedSourceCancelSafe<Rtp>),
    RemoveSender,

    SetReceiver(mpsc::Sender<RtpPacket>),
    RemoveReceiver,
}

compile_error!(
    "rework this task to be generic over a RTP transport and contain one or many rtp sessions"
);

/// RTP task which sends and receives RTP & RTCP packets using the inner UdpSockets
struct Task {
    state: TaskState,

    session: RtpSession,

    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Option<Arc<UdpSocket>>,

    remote_rtp_address: SocketAddr,
    remote_rtcp_address: SocketAddr,

    /// Reusable buffer to serialize RTP/RTCP packets into
    encode_buf: Vec<u8>,

    /// The RTP task polls this source yielding RTP packets, which are then sent out using the `rtp_socket` to `remote_rtp_addr`
    rtp_sender_source: Option<BoxedSourceCancelSafe<Rtp>>,

    /// Sink to dump RTP packets into, that have been received on the `rtp_socket`
    rtp_receiver_sink: Option<mpsc::Sender<RtpPacket>>,

    /// Channel to receive commands from the RtpSessions object from. When this returns None, the Task quits
    command_rx: mpsc::Receiver<ToTaskCommand>,
}

enum TaskState {
    Ok,
    ExitOk,
    ExitErr,
}

impl Task {
    async fn run(mut self) {
        let mut rtcp_interval = interval_at(
            Instant::now() + Duration::from_secs(5),
            Duration::from_secs(5),
        );

        let mut rtp_recv_buf = vec![0u8; RECV_BUFFER_SIZE];
        let mut rtcp_recv_buf = vec![0u8; RECV_BUFFER_SIZE];

        // TODO: expand the jitterbuffer API to know when to poll
        let mut poll_jitterbuffer_interval = interval(Duration::from_millis(10));

        while let TaskState::Ok = self.state {
            // Reset send & receive buffers
            self.encode_buf.clear();
            rtp_recv_buf.resize(RECV_BUFFER_SIZE, 0);
            rtcp_recv_buf.resize(RECV_BUFFER_SIZE, 0);

            select! {
                // Wait for external commands
                command = self.command_rx.recv() => self.handle_command(command),

                // Check the jitterbuffer in a fixed interval
                _ = poll_jitterbuffer_interval.tick() => self.poll_jitterbuffer().await,

                // Wait for the application to generate RTP packets and send them out
                event = poll_source(&mut self.rtp_sender_source) => self.handle_rtp_source_event(event).await,

                // Send out RTCP packets in a fixed interval
                _ = rtcp_interval.tick() => self.send_rtcp().await,

                // Receive RTP or RTCP packets from the rtp-socket
                result = self.rtp_socket.recv_from(&mut rtp_recv_buf) => self.handle_rtp_packet_recv(&rtp_recv_buf, result),

                // Receive RTCP packet if the RTCP socket exists
                result = poll_rtcp_socket(self.rtcp_socket.as_deref(), &mut rtcp_recv_buf) => self.handle_rtcp_packet_recv(&rtcp_recv_buf, result),
            }
        }

        match self.state {
            TaskState::Ok => unreachable!(),
            TaskState::ExitErr => log::warn!("exited rtp task, due to error"),
            TaskState::ExitOk => log::debug!("exited rtp task gracefully"),
        }
    }

    fn handle_command(&mut self, command: Option<ToTaskCommand>) {
        let Some(command) = command else {
            self.state = TaskState::ExitOk;
            return;
        };

        match command {
            ToTaskCommand::SetSender(source) => self.rtp_sender_source = Some(source),
            ToTaskCommand::RemoveSender => self.rtp_sender_source = None,
            ToTaskCommand::SetReceiver(sender) => self.rtp_receiver_sink = Some(sender),
            ToTaskCommand::RemoveReceiver => self.rtp_receiver_sink = None,
        }
    }

    async fn poll_jitterbuffer(&mut self) {
        while let Some(packet) = self.session.pop_rtp(None) {
            let Some(tx) = &self.rtp_receiver_sink else {
                continue;
            };

            if tx.send(packet).await.is_err() {
                log::warn!("Failed to forward incoming rtp packet, receiver might have been dropped prematurely");
                self.rtp_receiver_sink = None;
            }
        }
    }

    async fn handle_rtp_source_event(&mut self, event: ezk::Result<SourceEvent<Rtp>>) {
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                log::error!("rtp task's source encountered an error, removing source - {e:?}");
                self.rtp_sender_source = None;
                return;
            }
        };

        let frame = match event {
            SourceEvent::Frame(frame) => frame,
            SourceEvent::RenegotiationNeeded => {
                unreachable!("rtp sources should not need renegotiation");
            }
            SourceEvent::EndOfData => {
                log::debug!("rtp source end of data, removing");
                self.rtp_sender_source = None;
                return;
            }
        };

        let mut packet = frame.into_data();
        let mut packet_mut = packet.get_mut();

        // Set missing packet header fields
        packet_mut.set_ssrc(self.session.ssrc());
        packet_mut
            .as_builder()
            .write_into_vec(&mut self.encode_buf)
            .expect("buffer of 65535 bytes must be large enough");

        self.session.send_rtp(&packet);

        if let Err(e) = self
            .rtp_socket
            .send_to(&self.encode_buf, self.remote_rtp_address)
            .await
        {
            log::warn!(
                "Failed to send RTP packet of length={} to address={}, {e}",
                self.encode_buf.len(),
                self.remote_rtp_address
            );
            self.state = TaskState::ExitErr;
        }
    }

    async fn send_rtcp(&mut self) {
        // make_rtcp needs a mut slice to write into, resize encode buf accordingly
        self.encode_buf.resize(65535, 0);
        let len = match self.session.write_rtcp_report(&mut self.encode_buf) {
            Ok(len) => len,
            Err(e) => {
                log::warn!("Failed to write RTCP packet, {e:?}");
                return;
            }
        };
        self.encode_buf.truncate(len);

        let rtcp_socket = self.rtcp_socket.as_ref().unwrap_or(&self.rtp_socket);

        if let Err(e) = rtcp_socket
            .send_to(&self.encode_buf, self.remote_rtcp_address)
            .await
        {
            log::warn!(
                "Failed to send RTCP packet of length={} to address={}, {e}",
                self.encode_buf.len(),
                self.remote_rtp_address
            );
            self.state = TaskState::ExitErr;
        }
    }

    fn handle_rtp_packet_recv(&mut self, buf: &[u8], result: io::Result<(usize, SocketAddr)>) {
        let Some(len) = self.handle_recv_io_result(result) else {
            return;
        };

        if len < 2 {
            return;
        }

        let buf = &buf[..len];
        let pt = buf[1];

        // Test if the packet's payload type is inside the forbidden range, which would make it a RTCP packet
        if let 64..=95 = pt & 0x7F {
            // This is most likely a RTCP packet
            self.handle_rtcp(buf);
        } else {
            // This is most likely a RTP packet
            match RtpPacket::parse(buf) {
                Ok(pkg) => self.session.recv_rtp(pkg),
                Err(e) => log::debug!("Failed to parse RTP packet, {e:?}"),
            }
        }
    }

    fn handle_rtcp_packet_recv(&mut self, buf: &[u8], result: io::Result<(usize, SocketAddr)>) {
        let Some(len) = self.handle_recv_io_result(result) else {
            return;
        };

        self.handle_rtcp(&buf[..len]);
    }

    fn handle_rtcp(&mut self, buf: &[u8]) {
        let rtcp_compound = match Compound::parse(buf) {
            Ok(rtcp_compound) => rtcp_compound,
            Err(e) => {
                log::debug!("Failed to parse incoming RTCP packet, {e}");
                return;
            }
        };

        for pkg in rtcp_compound {
            match pkg {
                Ok(packet) => self.session.recv_rtcp(packet),
                Err(e) => log::debug!("Failed to parse RTCP packet in compound packet, {e:?}"),
            }
        }
    }

    fn handle_recv_io_result(&mut self, result: io::Result<(usize, SocketAddr)>) -> Option<usize> {
        match result {
            Ok((len, _)) => Some(len),
            Err(e) => {
                log::warn!("Failed to read from udpsocket, {e}");
                self.state = TaskState::ExitErr;
                None
            }
        }
    }
}

async fn poll_rtcp_socket(
    socket: Option<&UdpSocket>,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr)> {
    if let Some(socket) = socket {
        socket.recv_from(buf).await
    } else {
        pending().await
    }
}

async fn poll_source(
    source: &mut Option<BoxedSourceCancelSafe<Rtp>>,
) -> ezk::Result<SourceEvent<Rtp>> {
    match source {
        Some(source) => source.next_event().await,
        None => pending().await,
    }
}
