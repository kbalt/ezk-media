use crate::{
    libg722::{encoder::Encoder, Bitrate},
    G722Config, G722ConfigRange, G722,
};
use ezk::{
    ConfigRange, Error, Frame, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent,
    ValueRange,
};
use ezk_audio::{Channels, Format, RawAudio, RawAudioConfigRange, SampleRate, Samples};

pub struct G722Encoder<S> {
    source: S,
    stream: Option<Stream>,
}

struct Stream {
    encoder: Encoder,
}

impl<S> NextEventIsCancelSafe for G722Encoder<S> where
    S: Source<MediaType = RawAudio> + NextEventIsCancelSafe
{
}

impl<S> G722Encoder<S>
where
    S: Source<MediaType = RawAudio>,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            stream: None,
        }
    }

    fn upstream_config_range(&self) -> RawAudioConfigRange {
        RawAudioConfigRange {
            sample_rate: ValueRange::Value(SampleRate(16000)),
            channels: ValueRange::Value(Channels::NotPositioned(1)),
            format: ValueRange::Value(Format::I16),
        }
    }

    async fn find_compatible_config(&mut self) -> Result<RawAudioConfigRange> {
        let capabilities = self.source.capabilities().await?;

        let compatible_config = self.upstream_config_range();

        capabilities
            .iter()
            .find_map(|c| c.intersect(&compatible_config))
            .ok_or_else(|| Error::negotiation_failed(capabilities, vec![compatible_config]))
    }
}

impl<S> Source for G722Encoder<S>
where
    S: Source<MediaType = RawAudio>,
{
    type MediaType = G722;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        // assert that the source has a compatible config
        self.find_compatible_config().await?;

        Ok(vec![G722ConfigRange {}])
    }

    async fn negotiate_config(&mut self, _available: Vec<G722ConfigRange>) -> Result<G722Config> {
        let range = self.find_compatible_config().await?;

        self.source.negotiate_config(vec![range]).await?;

        self.stream = Some(Stream {
            encoder: Encoder::new(Bitrate::Mode1_64000, false, false),
        });

        Ok(G722Config {})
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        match self.source.next_event().await? {
            SourceEvent::Frame(frame) => {
                let Samples::I16(samples) = &frame.data().samples else {
                    unreachable!()
                };

                Ok(SourceEvent::Frame(Frame::new(
                    stream.encoder.encode(samples).into(),
                    frame.timestamp,
                )))
            }
            SourceEvent::EndOfData => Ok(SourceEvent::EndOfData),
            SourceEvent::RenegotiationNeeded => Ok(SourceEvent::RenegotiationNeeded),
        }
    }
}
