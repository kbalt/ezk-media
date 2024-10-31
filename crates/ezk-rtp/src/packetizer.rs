use crate::{Payloadable, Payloader, Rtp, RtpConfig, RtpConfigRange, RtpPacket};
use ezk::{
    ConfigRange, Frame, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent, ValueRange,
};
use std::collections::VecDeque;

pub struct Packetizer<S: Source<MediaType: Payloadable>> {
    source: S,

    stream: Option<Stream<S::MediaType>>,
}

impl<S: Source<MediaType: Payloadable> + NextEventIsCancelSafe> NextEventIsCancelSafe
    for Packetizer<S>
{
}

struct Stream<M: Payloadable> {
    config: RtpConfig,
    sequence_number: u16,

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
            stream: None,
        }
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
            .negotiate_config(vec![<S::MediaType as MediaType>::ConfigRange::any()])
            .await?;

        let pt = available[0].pt.first_value();

        let config = RtpConfig { pt };

        self.stream = Some(Stream {
            config,
            sequence_number: rand::random(),
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
                let timestamp = packet.get().timestamp();

                return Ok(SourceEvent::Frame(Frame::new(packet, timestamp as u64)));
            }

            match self.source.next_event().await? {
                SourceEvent::Frame(frame) => {
                    let timestamp = frame.timestamp as u32;

                    for payload in stream.payloader.payload(frame) {
                        stream.sequence_number = stream.sequence_number.wrapping_add(1);

                        let packet = RtpPacket::new(
                            &rtp_types::RtpPacketBuilder::new()
                                .sequence_number(stream.sequence_number)
                                .timestamp(timestamp)
                                .payload_type(stream.config.pt)
                                .payload(&payload),
                        );

                        stream.queue.push_back(packet);
                    }
                }
                SourceEvent::EndOfData => return Ok(SourceEvent::EndOfData),
                SourceEvent::RenegotiationNeeded => return Ok(SourceEvent::RenegotiationNeeded),
            }
        }
    }
}
