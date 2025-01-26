use crate::{
    ExtendedSequenceNumber, Payloadable, Payloader, Rtp, RtpConfig, RtpConfigRange, RtpExtensions,
    RtpPacket, RtpTimestamp, Ssrc,
};
use ezk::{ConfigRange, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent, ValueRange};
use std::collections::VecDeque;

pub struct Packetizer<S: Source<MediaType: Payloadable>> {
    source: S,
    mtu: usize,
    stream: Option<Stream<S::MediaType>>,
}

impl<S: Source<MediaType: Payloadable> + NextEventIsCancelSafe> NextEventIsCancelSafe
    for Packetizer<S>
{
}

struct Stream<M: Payloadable> {
    config: RtpConfig,
    sequence_number: ExtendedSequenceNumber,

    queue: VecDeque<RtpPacket>,
    payloader: M::Payloader,
}

impl<S> Packetizer<S>
where
    S: Source<MediaType: Payloadable>,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            mtu: 1400,
            stream: None,
        }
    }

    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }
}

impl<S> Source for Packetizer<S>
where
    S: Source<MediaType: Payloadable>,
{
    type MediaType = Rtp;

    async fn capabilities(&mut self) -> Result<Vec<RtpConfigRange>> {
        let _capabilities = self.source.capabilities().await?;

        if let Some(static_pt) = S::MediaType::STATIC_PT {
            Ok(vec![RtpConfigRange {
                pt: ValueRange::Value(static_pt),
            }])
        } else {
            Ok(vec![RtpConfigRange {
                pt: ValueRange::range(96, 127),
            }])
        }
    }

    async fn negotiate_config(&mut self, available: Vec<RtpConfigRange>) -> Result<RtpConfig> {
        let config_ = self
            .source
            .negotiate_config(vec![ConfigRange::any()])
            .await?;

        let pt = available[0].pt.first_value();

        let config = RtpConfig { pt };

        self.stream = Some(Stream {
            config,
            sequence_number: ExtendedSequenceNumber((rand::random::<u16>() / 2).into()),
            queue: VecDeque::new(),
            payloader: S::MediaType::make_payloader(config_),
        });

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        loop {
            if let Some(packet) = stream.queue.pop_front() {
                let timestamp = packet.timestamp;

                return Ok(SourceEvent::Frame(Frame::new(packet, timestamp.0 as u64)));
            }

            let frame = match self.source.next_event().await? {
                SourceEvent::Frame(frame) => frame,
                SourceEvent::EndOfData => return Ok(SourceEvent::EndOfData),
                SourceEvent::RenegotiationNeeded => return Ok(SourceEvent::RenegotiationNeeded),
            };

            let timestamp = (frame.timestamp & u64::from(u32::MAX)) as u32;

            for payload in stream.payloader.payload(frame, self.mtu) {
                let packet = RtpPacket {
                    pt: stream.config.pt,
                    sequence_number: stream.sequence_number.increase_one(),
                    ssrc: Ssrc(0), // this is set later
                    timestamp: RtpTimestamp(timestamp),
                    extensions: RtpExtensions::default(),
                    payload,
                };

                stream.queue.push_back(packet);
            }
        }
    }
}
