use super::{RtpTransport, WhichTransport, RECV_BUFFER_SIZE};
use crate::{ActiveMediaId, RTP_MID_HDREXT_ID};
use bytesstr::BytesStr;
use ezk_rtp::rtcp_types::{self, Compound};
use ezk_rtp::{parse_extensions, RtpExtensionsWriter, RtpPacket, RtpSession};
use std::collections::HashMap;
use std::future::{pending, poll_fn};
use std::net::SocketAddr;
use std::task::Poll;
use std::time::Duration;
use std::{fmt, io};
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, interval_at, Instant};

// TODO: name
// TODO: https://www.rfc-editor.org/rfc/rfc8843#section-9.2
// TODO: https://www.rfc-editor.org/rfc/rfc4585.html

/// See https://www.rfc-editor.org/rfc/rfc8843#section-9.2
#[derive(Debug)]
pub struct IdentifyableBy {
    pub mid: Option<BytesStr>,
    pub ssrc: Vec<u32>,
    pub pt: Vec<u8>,
}

pub(crate) struct TransportTaskHandle {
    command_tx: mpsc::Sender<ToTaskCommand>,
}

impl TransportTaskHandle {
    pub fn new<T: RtpTransport>(transport: T, mid_rtp_id: Option<u8>) -> Self {
        let (command_tx, command_rx) = mpsc::channel(1);

        tokio::spawn(
            TransportTask {
                state: TaskState::Ok,
                rtp_sessions: HashMap::new(),
                mid_rtp_id,
                transport,
                encode_buf: vec![],
                command_rx,
            }
            .run(),
        );

        Self { command_tx }
    }

    pub async fn add_media_session(
        &self,
        id: ActiveMediaId,
        remote_identifyable_by: IdentifyableBy,
        clock_rate: u32,
    ) {
        self.command_tx
            .send(ToTaskCommand::AddMediaSession {
                id,
                remote_identifyable_by,
                clock_rate,
            })
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn set_sender(&self, id: ActiveMediaId, receiver: mpsc::Receiver<RtpPacket>) {
        self.command_tx
            .send(ToTaskCommand::SetSender(id, receiver))
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn remove_sender(&self, id: ActiveMediaId) {
        self.command_tx
            .send(ToTaskCommand::RemoveSender(id))
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn set_receiver(&self, id: ActiveMediaId, sender: mpsc::Sender<RtpPacket>) {
        self.command_tx
            .send(ToTaskCommand::SetReceiver(id, sender))
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn remove_receiver(&self, id: ActiveMediaId) {
        self.command_tx
            .send(ToTaskCommand::RemoveReceiver(id))
            .await
            .expect("task must not exit while command_tx exists");
    }

    pub async fn remove_media_session(&self, id: ActiveMediaId) {
        self.command_tx
            .send(ToTaskCommand::RemoveMediaSession(id))
            .await
            .expect("task must not exit while command_tx exists");
    }
}

enum ToTaskCommand {
    AddMediaSession {
        id: ActiveMediaId,
        remote_identifyable_by: IdentifyableBy,
        clock_rate: u32,
    },
    RemoveMediaSession(ActiveMediaId),

    SetSender(ActiveMediaId, mpsc::Receiver<RtpPacket>),
    RemoveSender(ActiveMediaId),

    SetReceiver(ActiveMediaId, mpsc::Sender<RtpPacket>),
    RemoveReceiver(ActiveMediaId),
}

/// RTP task which sends and receives RTP & RTCP packets using the inner transport
struct TransportTask<T> {
    state: TaskState,

    rtp_sessions: HashMap<ActiveMediaId, Entry>,
    mid_rtp_id: Option<u8>,

    transport: T,

    /// Reusable buffer to serialize RTP/RTCP packets into
    encode_buf: Vec<u8>,

    /// Channel to receive commands from the RtpSessions object from. When this returns None, the Task quits
    command_rx: mpsc::Receiver<ToTaskCommand>,
}

struct Entry {
    rtp_session: RtpSession,
    remote_identifyable_by: IdentifyableBy,
    receiver_sender: Option<mpsc::Sender<RtpPacket>>,
    sender_receiver: Option<mpsc::Receiver<RtpPacket>>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("rtp_session", &self.rtp_session)
            .field("remote_identifyable_by", &self.remote_identifyable_by)
            .field("receiver_sender", &"[opaque]")
            .finish()
    }
}

enum TaskState {
    Ok,
    ExitOk,
    ExitErr,
}

impl<T: RtpTransport> TransportTask<T> {
    async fn run(mut self) {
        let mut rtcp_interval = interval_at(
            Instant::now() + Duration::from_secs(5000),
            Duration::from_secs(5),
        );

        // TODO: expand the jitterbuffer API to know when to poll
        let mut poll_jitterbuffer_interval = interval(Duration::from_secs(100));

        while let TaskState::Ok = self.state {
            // Reset send & receive buffers
            self.encode_buf.clear();

            select! {
                // Wait for external commands
                command = self.command_rx.recv() => self.handle_command(command),

                // Check the jitterbuffer in a fixed interval
                _ = poll_jitterbuffer_interval.tick() => self.poll_jitterbuffer().await,

                // Wait for the application to generate RTP packets and send them out, but only when the transport is ready
                (mid, event) = poll_session_sources(&mut self.rtp_sessions, self.transport.is_ready()) => {
                    self.send_rtp_packet(mid, event).await;
                }

                // Send out RTCP packets in a fixed interval
                _ = rtcp_interval.tick() => self.send_rtcp().await,

                // Receive event from transport & handle it
                result = self.transport.poll_event() => self.handle_recv(result).await,
            }
        }

        match self.state {
            TaskState::Ok => {}
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
            ToTaskCommand::AddMediaSession {
                id,
                remote_identifyable_by,
                clock_rate,
            } => {
                let mut rtp_session = RtpSession::new(rand::random(), clock_rate);
                if let Some(mid) = &remote_identifyable_by.mid {
                    rtp_session.add_source_description_item(15, None, mid.to_string());
                }

                self.rtp_sessions.insert(
                    id,
                    Entry {
                        rtp_session: RtpSession::new(rand::random(), clock_rate),
                        remote_identifyable_by,
                        receiver_sender: None,
                        sender_receiver: None,
                    },
                );
            }
            ToTaskCommand::RemoveMediaSession(id) => {
                self.rtp_sessions.remove(&id);
            }
            ToTaskCommand::SetSender(id, receiver) => {
                self.rtp_sessions.get_mut(&id).unwrap().sender_receiver = Some(receiver);
            }
            ToTaskCommand::RemoveSender(id) => {
                self.rtp_sessions.get_mut(&id).unwrap().sender_receiver = None;
            }
            ToTaskCommand::SetReceiver(id, sender) => {
                self.rtp_sessions.get_mut(&id).unwrap().receiver_sender = Some(sender)
            }
            ToTaskCommand::RemoveReceiver(id) => {
                self.rtp_sessions.get_mut(&id).unwrap().receiver_sender = None;
            }
        }
    }

    async fn poll_jitterbuffer(&mut self) {
        for entry in self.rtp_sessions.values_mut() {
            while let Some(packet) = entry.rtp_session.pop_rtp(None) {
                let Some(tx) = &entry.receiver_sender else {
                    continue;
                };

                if tx.send(packet).await.is_err() {
                    log::warn!("Failed to forward incoming rtp packet, receiver might have been dropped prematurely");
                    entry.receiver_sender = None;
                }
            }
        }
    }

    async fn send_rtp_packet(&mut self, id: ActiveMediaId, mut packet: RtpPacket) {
        let mut packet_mut = packet.get_mut();
        let entry = self.rtp_sessions.get_mut(&id).unwrap();

        {
            // Set missing packet header fields
            packet_mut.set_ssrc(entry.rtp_session.ssrc());
            let mut builder = packet_mut.as_builder();

            let mut extension_data = vec![];
            if let Some(mid) = &entry.remote_identifyable_by.mid {
                let profile = RtpExtensionsWriter::new(&mut extension_data, mid.len() <= 16)
                    .with(RTP_MID_HDREXT_ID, mid.as_bytes())
                    .finish();

                builder = builder.extension(profile, &extension_data);
            }

            builder
                .write_into_vec(&mut self.encode_buf)
                .expect("buffer of 65535 bytes must be large enough");
        }

        entry.rtp_session.send_rtp(&packet);

        if let Err(e) = self.transport.send_rtp(&mut self.encode_buf).await {
            log::warn!(
                "Failed to send RTP packet of length={}, {e}",
                self.encode_buf.len(),
            );
            self.state = TaskState::ExitErr;
        }
    }

    async fn send_rtcp(&mut self) {
        for entry in self.rtp_sessions.values_mut() {
            // make_rtcp needs a mut slice to write into, resize encode buf accordingly
            self.encode_buf.resize(65535, 0);

            let len = match entry.rtp_session.write_rtcp_report(&mut self.encode_buf) {
                Ok(len) => len,
                Err(e) => {
                    log::warn!("Failed to write RTCP packet, {e:?}");
                    return;
                }
            };

            self.encode_buf.truncate(len);

            if let Err(e) = self.transport.send_rtcp(&mut self.encode_buf).await {
                log::warn!(
                    "Failed to send RTCP packet of length={} {e}",
                    self.encode_buf.len(),
                );
                self.state = TaskState::ExitErr;
            }
        }
    }

    async fn handle_recv(&mut self, result: io::Result<T::Event>) {
        let event = match result {
            Ok(event) => event,
            Err(e) => {
                log::warn!("Failed to recv from transport, {e}");
                self.state = TaskState::ExitErr;
                return;
            }
        };

        let buf = match self.transport.handle_event(event).await {
            Ok(Some(buf)) => buf,
            Ok(None) => return,
            Err(e) => {
                log::warn!("Failed to handle_recv, {e}");
                self.state = TaskState::ExitErr;
                return;
            }
        };

        if buf.len() < 2 {
            return;
        }

        let pt = buf[1];

        // Test if the packet's payload type is inside the forbidden range, which would make it a RTCP packet
        if let 64..=95 = pt & 0x7F {
            // This is most likely a RTCP packet
            let rtcp_compound = match Compound::parse(buf) {
                Ok(rtcp_compound) => rtcp_compound,
                Err(e) => {
                    log::debug!("Failed to parse incoming RTCP packet, {e}");
                    return;
                }
            };

            // self.handle_rtcp_compound(rtcp_compound);
        } else {
            // This is most likely a RTP packet
            let rtp_packet = match RtpPacket::parse(buf) {
                Ok(rtp_packet) => rtp_packet,
                Err(e) => {
                    log::debug!("Failed to parse RTP packet, {e:?}");
                    return;
                }
            };

            self.handle_rtp_packet(rtp_packet);
        }
    }

    fn handle_rtcp_compound(&mut self, rtcp_compound: Compound<'_>) {
        println!("Received RTCP");
        for pkg in rtcp_compound {
            let packet = match pkg {
                Ok(packet) => packet,
                Err(e) => {
                    log::debug!("Failed to parse RTCP packet in compound packet, {e:?}");
                    return;
                }
            };

            match &packet {
                rtcp_types::Packet::App(_app) => {}
                rtcp_types::Packet::Bye(_bye) => {
                    // TODO: handle bye
                }
                rtcp_types::Packet::Rr(_receiver_report) => {
                    // let entry = ...
                    // entry.rtp_session.recv_rtcp(packet);
                }
                rtcp_types::Packet::Sdes(_sdes) => {
                    // TODO
                }
                rtcp_types::Packet::Sr(_sender_report) => {
                    // let entry = ...
                    // entry.rtp_session.recv_rtcp(packet);
                }
                rtcp_types::Packet::TransportFeedback(_transport_feedback) => {}
                rtcp_types::Packet::PayloadFeedback(_payload_feedback) => {}
                rtcp_types::Packet::Unknown(_) => {}
            }
        }
    }

    fn handle_rtp_packet(&mut self, rtp_packet: RtpPacket) {
        let pkg = rtp_packet.get();

        // Parse the mid from the rtp-packet's extensions
        let mid = self
            .mid_rtp_id
            .zip(pkg.extension())
            .and_then(|(mid_rtp_id, (profile, data))| {
                parse_extensions(profile, data)
                    .find(|(id, _)| *id == mid_rtp_id)
                    .map(|(_, mid)| mid)
            });

        let entry =
            self.rtp_sessions
                .values_mut()
                .find(|e| match (&e.remote_identifyable_by.mid, mid) {
                    (Some(a), Some(b)) => a.as_bytes() == b,
                    (None, None) => e.remote_identifyable_by.ssrc.contains(&pkg.ssrc()),
                    _ => false,
                });

        let entry = if let Some(entry) = entry {
            Some(entry)
        } else {
            // Try to search for a matching payload type
            self.rtp_sessions
                .values_mut()
                .find(|e| e.remote_identifyable_by.pt.contains(&pkg.payload_type()))
        };

        if let Some(entry) = entry {
            entry.rtp_session.recv_rtp(rtp_packet);
        }
    }
}

async fn poll_session_sources(
    entries: &mut HashMap<ActiveMediaId, Entry>,
    transport_is_ready: bool,
) -> (ActiveMediaId, RtpPacket) {
    if entries.is_empty() || !transport_is_ready {
        return pending().await;
    }

    poll_fn(|cx| {
        for (id, entry) in &mut *entries {
            let Some(receiver) = &mut entry.sender_receiver else {
                continue;
            };

            match receiver.poll_recv(cx) {
                Poll::Ready(Some(packet)) => return Poll::Ready((*id, packet)),
                Poll::Ready(None) => entry.sender_receiver = None,
                Poll::Pending => {}
            }
        }

        Poll::Pending
    })
    .await
}
