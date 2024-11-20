use crate::{NtpTimestamp, RtpPacket};
use jitter_buffer::{guess_timestamp, JitterBuffer};
use rtcp_types::{
    CompoundBuilder, ReceiverReport, ReportBlock, RtcpPacketWriterExt, RtcpWriteError, SdesBuilder,
    SdesChunkBuilder, SdesItemBuilder, SenderReport,
};
use std::time::{Duration, Instant};
use time::ext::InstantExt;

mod jitter_buffer;

const DEFAULT_JITTERBUFFER_LENGTH: Duration = Duration::from_millis(100);

/// Single RTP session, (1 sender, many receiver)
///
/// This can be used to publish a single RTP source and receive others.
/// It manages a jitterbuffer for every remote ssrc and can generate RTCP reports.
pub struct RtpSession {
    ssrc: u32,
    clock_rate: u32,

    /// tag/type, prefix, value
    source_description_items: Vec<(u8, Option<Vec<u8>>, String)>,

    sender: Option<SenderState>,
    receiver: Vec<ReceiverState>,
}

struct SenderState {
    ntp_timestamp: NtpTimestamp,
    rtp_timestamp: u64,

    sender_pkg_count: u32,
    sender_octet_count: u32,
}

#[derive(Default)]
struct ReceiverState {
    ssrc: u32,

    jitter_buffer: JitterBuffer,

    last_rtp_received: Option<(Instant, u64)>,
    jitter: f32,

    last_sr: Option<NtpTimestamp>,
    total_lost: u64,
}

impl RtpSession {
    pub fn new(ssrc: u32, clock_rate: u32) -> Self {
        Self {
            ssrc,
            source_description_items: vec![],
            clock_rate,
            sender: None,
            receiver: vec![],
        }
    }

    /// Add an item to the RTCP packets source description
    pub fn with_source_description_item(
        mut self,
        tag: u8,
        prefix: Option<Vec<u8>>,
        value: String,
    ) -> Self {
        self.add_source_description_item(tag, prefix, value);
        self
    }

    /// Add an item to the RTCP packets source description
    pub fn add_source_description_item(&mut self, tag: u8, prefix: Option<Vec<u8>>, value: String) {
        self.source_description_items.push((tag, prefix, value));
    }

    /// Sender ssrc of this session
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Clock rate of the RTP timestamp
    pub fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    /// Register an RTP packet before sending it out
    pub fn send_rtp(&mut self, packet: &RtpPacket) {
        let packet = packet.get();

        let sender_status = self.sender.get_or_insert(SenderState {
            ntp_timestamp: NtpTimestamp::ZERO,
            rtp_timestamp: 0,

            sender_pkg_count: 0,
            sender_octet_count: 0,
        });

        sender_status.ntp_timestamp = NtpTimestamp::now();
        sender_status.rtp_timestamp =
            guess_timestamp(sender_status.rtp_timestamp, packet.timestamp());

        sender_status.sender_pkg_count += 1;
        sender_status.sender_octet_count += packet.payload_len() as u32;
    }

    /// Receive an RTP packet.
    ///
    /// The session consumes the packet and puts in into a internal jitterbuffer to fix potential reordering.
    pub fn recv_rtp(&mut self, rtp_packet: RtpPacket) {
        let packet = rtp_packet.get();

        let receiver_status = if let Some(receiver_status) =
            self.receiver.iter_mut().find(|r| r.ssrc == packet.ssrc())
        {
            receiver_status
        } else {
            // Don't allow an infinite amount of receivers
            if self.receiver.len() > 4096 {
                return;
            }

            self.receiver.push(ReceiverState {
                ssrc: packet.ssrc(),
                jitter_buffer: JitterBuffer::default(),
                last_rtp_received: None,
                jitter: 0.0,
                last_sr: None,
                total_lost: 0,
            });

            self.receiver.last_mut().unwrap()
        };

        let now = Instant::now();

        // Update jitter and find extended timestamp
        let timestamp = if let Some((last_rtp_instant, last_rtp_timestamp)) =
            receiver_status.last_rtp_received
        {
            // Rj - Ri
            let a = now - last_rtp_instant;
            let a = (a.as_secs_f32() * self.clock_rate as f32) as i64;

            // Sj - Si
            let b = packet.timestamp() as i64 - last_rtp_timestamp as i64;

            // (Rj - Ri) - (Sj - Si)
            let d = a.abs_diff(b);

            receiver_status.jitter =
                receiver_status.jitter + ((d as f32).abs() - receiver_status.jitter) / 16.;

            guess_timestamp(last_rtp_timestamp, packet.timestamp())
        } else {
            packet.timestamp() as u64
        };

        receiver_status.last_rtp_received = Some((now, timestamp));

        receiver_status.jitter_buffer.push(rtp_packet);
    }

    pub fn pop_rtp(&mut self, jitter_buffer_length: Option<Duration>) -> Option<RtpPacket> {
        let pop_earliest =
            Instant::now() - jitter_buffer_length.unwrap_or(DEFAULT_JITTERBUFFER_LENGTH);

        for receiver in &mut self.receiver {
            let Some((last_rtp_received_instant, last_rtp_received_timestamp)) =
                receiver.last_rtp_received
            else {
                continue;
            };

            let max_timestamp = map_instant_to_rtp_timestamp(
                last_rtp_received_instant,
                last_rtp_received_timestamp,
                self.clock_rate,
                pop_earliest,
            );

            if let Some(packet) = receiver.jitter_buffer.pop(max_timestamp) {
                return Some(packet);
            }
        }

        None
    }

    pub fn recv_rtcp(&mut self, packet: rtcp_types::Packet<'_>) {
        // TODO: read reports
        if let rtcp_types::Packet::Sr(sr) = packet {
            if let Some(receiver) = self
                .receiver
                .iter_mut()
                .find(|status| status.ssrc == sr.ssrc())
            {
                receiver.last_sr = Some(NtpTimestamp::now());
            }
        }
    }

    /// Generate RTCP sender or receiver report packet.
    ///
    /// This resets the internal received & lost packets counter for every receiver.
    pub fn write_rtcp_report(&mut self, dst: &mut [u8]) -> Result<usize, RtcpWriteError> {
        let now = NtpTimestamp::now();

        let mut report_blocks = vec![];

        for receiver in &mut self.receiver {
            let lost = receiver.jitter_buffer.lost;
            let received = receiver.jitter_buffer.received;

            receiver.total_lost += lost;
            receiver.jitter_buffer.lost = 0;
            receiver.jitter_buffer.received = 0;

            let fraction_lost = (lost as f64 / (received + lost) as f64) * 255.0;
            let fraction_lost = fraction_lost as u32;

            let (last_sr, delay) = if let Some(last_sr) = receiver.last_sr {
                let delay = now - last_sr;
                let delay = (delay.as_seconds_f64() * 65536.0) as u32;

                let last_sr = last_sr.to_fixed_u32();

                (last_sr, delay)
            } else {
                (0, 0)
            };

            let last_sequence_number = receiver
                .jitter_buffer
                .last_sequence_number()
                .unwrap_or_default();

            let report_block = ReportBlock::builder(receiver.ssrc)
                .fraction_lost(fraction_lost as u8)
                .cumulative_lost(receiver.total_lost as u32)
                .extended_sequence_number(lower_32bits(last_sequence_number))
                .interarrival_jitter(receiver.jitter as u32)
                .last_sender_report_timestamp(last_sr)
                .delay_since_last_sender_report_timestamp(delay);

            report_blocks.push(report_block);
        }

        let mut compound = CompoundBuilder::default();

        // Add report block
        if let Some(sender_info) = &self.sender {
            let rtp_timestamp = {
                let offset = (self.clock_rate * (now - sender_info.ntp_timestamp)).as_seconds_f64()
                    * self.clock_rate as f64;
                sender_info.rtp_timestamp + offset as u64
            };

            let mut sr = SenderReport::builder(self.ssrc)
                .ntp_timestamp(now.to_fixed_u64())
                .rtp_timestamp(lower_32bits(rtp_timestamp))
                .packet_count(sender_info.sender_pkg_count)
                .octet_count(sender_info.sender_octet_count);

            for report_blocks in report_blocks {
                sr = sr.add_report_block(report_blocks);
            }

            compound = compound.add_packet(sr);
        } else {
            let mut rr = ReceiverReport::builder(self.ssrc);

            for report_blocks in report_blocks {
                rr = rr.add_report_block(report_blocks);
            }

            compound = compound.add_packet(rr);
        }

        // Add source description block
        if !self.source_description_items.is_empty() {
            let mut chunk = SdesChunkBuilder::new(self.ssrc);

            for (tag, prefix, value) in &self.source_description_items {
                let mut item = SdesItemBuilder::new(*tag, value);

                if let Some(prefix) = prefix {
                    item = item.prefix(prefix);
                }

                chunk = chunk.add_item(item);
            }

            compound = compound.add_packet(SdesBuilder::default().add_chunk(chunk));
        };

        // write into dst
        compound.write_into(dst)
    }
}

fn map_instant_to_rtp_timestamp(
    reference_instant: Instant,
    reference_timestamp: u64,
    clock_rate: u32,
    instant: Instant,
) -> u64 {
    let delta = instant.signed_duration_since(reference_instant);
    let delta_in_rtp_timesteps = (delta.as_seconds_f32() * clock_rate as f32) as i64;
    (reference_timestamp as i64 + delta_in_rtp_timesteps) as u64
}

fn lower_32bits(i: u64) -> u32 {
    (i & u64::from(u32::MAX)) as u32
}
