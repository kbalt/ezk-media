use crate::{DePayloader, Payloadable, Rtp};
use ezk::{ConfigRange, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent};

pub struct DePacketizer<S: Source<MediaType = Rtp>, M: Payloadable> {
    source: S,

    stream: Option<Stream<M>>,
}

impl<S: Source<MediaType = Rtp> + NextEventIsCancelSafe, M: Payloadable> NextEventIsCancelSafe
    for DePacketizer<S, M>
{
}

struct Stream<M: Payloadable> {
    depayloader: M::DePayloader,
}

impl<S, M> DePacketizer<S, M>
where
    S: Source<MediaType = Rtp>,
    M: Payloadable,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            stream: None,
        }
    }
}

impl<S, M> Source for DePacketizer<S, M>
where
    S: Source<MediaType = Rtp>,
    M: Payloadable,
{
    type MediaType = M;

    async fn capabilities(&mut self) -> Result<Vec<M::ConfigRange>> {
        let _capabilities = self.source.capabilities().await?;

        Ok(vec![M::ConfigRange::any()])
    }

    async fn negotiate_config(&mut self, available: Vec<M::ConfigRange>) -> Result<M::Config> {
        let (config, depayloader) = M::make_depayloader(available);

        self.stream = Some(Stream { depayloader });

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        let frame = match self.source.next_event().await? {
            SourceEvent::Frame(frame) => frame,
            SourceEvent::EndOfData => return Ok(SourceEvent::EndOfData),
            SourceEvent::RenegotiationNeeded => return Ok(SourceEvent::RenegotiationNeeded),
        };

        let frame_timestamp = frame.timestamp;
        let rtp_packet = frame.into_data();

        let data = stream.depayloader.depayload(rtp_packet.get().payload());

        Ok(SourceEvent::Frame(Frame::new(data, frame_timestamp)))
    }
}
