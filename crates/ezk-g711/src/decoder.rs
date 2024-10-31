use std::marker::PhantomData;

use crate::PCMX;
use ezk::{
    ConfigRange, Error, Frame, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent,
    ValueRange,
};
use ezk_audio::{
    Channels, Format, RawAudio, RawAudioConfig, RawAudioConfigRange, RawAudioFrame, SampleRate,
    Samples,
};

pub struct G711Decoder<S, M> {
    source: S,
    config: Option<RawAudioConfig>,
    _m: PhantomData<fn() -> M>,
}

impl<S, M> NextEventIsCancelSafe for G711Decoder<S, M>
where
    S: Source<MediaType = M> + NextEventIsCancelSafe,
    M: PCMX,
{
}

impl<S, M> G711Decoder<S, M>
where
    S: Source<MediaType = M>,
    M: PCMX,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            config: None,
            _m: PhantomData,
        }
    }

    fn downstream_config(&self) -> RawAudioConfigRange {
        RawAudioConfigRange {
            sample_rate: ValueRange::Value(SampleRate(8000)),
            channels: ValueRange::Value(Channels::NotPositioned(1)),
            format: ValueRange::Value(Format::I16),
        }
    }
}

impl<S, M> Source for G711Decoder<S, M>
where
    S: Source<MediaType = M>,
    M: PCMX,
{
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        // just making sure upstream doesn't error
        self.source.capabilities().await?;

        Ok(vec![self.downstream_config()])
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        let only_valid_config = self.downstream_config();

        let range = if let Some(range) = available
            .iter()
            .find_map(|c| c.intersect(&only_valid_config))
        {
            range
        } else {
            return Err(Error::msg("no valid config for G711Decoder"));
        };

        let config = RawAudioConfig {
            sample_rate: range.sample_rate.first_value(),
            channels: range.channels.first_value(),
            format: range.format.first_value(),
        };

        self.source
            .negotiate_config(vec![<S::MediaType as MediaType>::ConfigRange::any()])
            .await?;

        self.config = Some(config.clone());

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(config) = &self.config else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        match self.source.next_event().await? {
            SourceEvent::Frame(frame) => {
                let data = frame.data();

                let samples = Samples::from(S::MediaType::decode(data));

                Ok(SourceEvent::Frame(Frame::new(
                    RawAudioFrame {
                        sample_rate: config.sample_rate,
                        channels: config.channels.clone(),
                        samples,
                    },
                    frame.timestamp,
                )))
            }
            SourceEvent::EndOfData => {
                self.config = None;
                Ok(SourceEvent::EndOfData)
            }
            SourceEvent::RenegotiationNeeded => {
                self.config = None;
                Ok(SourceEvent::RenegotiationNeeded)
            }
        }
    }
}
