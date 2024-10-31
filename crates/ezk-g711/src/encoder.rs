use crate::PCMX;
use ezk::{
    ConfigRange, Error, Frame, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent,
    ValueRange,
};
use ezk_audio::{Channels, Format, RawAudio, RawAudioConfigRange, SampleRate, Samples};
use std::marker::PhantomData;

pub struct G711Encoder<S, M> {
    source: S,

    _m: PhantomData<fn() -> M>,
}

impl<S, M> NextEventIsCancelSafe for G711Encoder<S, M> where
    S: Source<MediaType = RawAudio> + NextEventIsCancelSafe
{
}

impl<S, M> G711Encoder<S, M>
where
    S: Source<MediaType = RawAudio>,
    M: PCMX,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            _m: PhantomData,
        }
    }

    fn raw_audio_config_range(&self) -> RawAudioConfigRange {
        RawAudioConfigRange {
            sample_rate: ValueRange::Value(SampleRate(8000)),
            channels: ValueRange::Value(Channels::NotPositioned(1)),
            format: ValueRange::Value(Format::I16),
        }
    }

    async fn find_compatible_config(&mut self) -> Result<RawAudioConfigRange> {
        let capabilities = self.source.capabilities().await?;

        let compatible_config = self.raw_audio_config_range();

        capabilities
            .into_iter()
            .find_map(|c| c.intersect(&compatible_config))
            .ok_or_else(|| Error::msg("G711Encoder couldn't find a compatible upstream config"))
    }
}

impl<S, M> Source for G711Encoder<S, M>
where
    S: Source<MediaType = RawAudio>,
    M: PCMX,
{
    type MediaType = M;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        // assert that the source has a compatible config
        self.find_compatible_config().await?;

        Ok(vec![M::ConfigRange::any()])
    }

    async fn negotiate_config(&mut self, _available: Vec<M::ConfigRange>) -> Result<M::Config> {
        let range = self.find_compatible_config().await?;

        self.source.negotiate_config(vec![range]).await?;

        Ok(M::Config::default())
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        match self.source.next_event().await? {
            SourceEvent::Frame(frame) => {
                let Samples::I16(samples) = &frame.data().samples else {
                    unreachable!()
                };

                Ok(SourceEvent::Frame(Frame::new(
                    M::encode(samples).into(),
                    frame.timestamp,
                )))
            }
            SourceEvent::EndOfData => Ok(SourceEvent::EndOfData),
            SourceEvent::RenegotiationNeeded => Ok(SourceEvent::RenegotiationNeeded),
        }
    }
}
