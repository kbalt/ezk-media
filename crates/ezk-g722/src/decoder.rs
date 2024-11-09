use crate::{
    libg722::{decoder::Decoder, Bitrate},
    G722ConfigRange, G722,
};
use ezk::{
    ConfigRange, Error, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent, ValueRange,
};
use ezk_audio::{
    Channels, Format, RawAudio, RawAudioConfig, RawAudioConfigRange, RawAudioFrame, SampleRate,
    Samples,
};

pub struct G722Decoder<S> {
    source: S,
    stream: Option<Stream>,
}

struct Stream {
    config: RawAudioConfig,
    decoder: Decoder,
}

impl<S> NextEventIsCancelSafe for G722Decoder<S> where
    S: Source<MediaType = G722> + NextEventIsCancelSafe
{
}

impl<S> G722Decoder<S>
where
    S: Source<MediaType = G722>,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            stream: None,
        }
    }

    fn downstream_config(&self) -> RawAudioConfigRange {
        RawAudioConfigRange {
            sample_rate: ValueRange::Value(SampleRate(16000)),
            channels: ValueRange::Value(Channels::NotPositioned(1)),
            format: ValueRange::Value(Format::I16),
        }
    }
}

impl<S> Source for G722Decoder<S>
where
    S: Source<MediaType = G722>,
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
            return Err(Error::msg("no valid config for G722Decoder"));
        };

        let config = RawAudioConfig {
            sample_rate: range.sample_rate.first_value(),
            channels: range.channels.first_value(),
            format: range.format.first_value(),
        };

        self.source
            .negotiate_config(vec![G722ConfigRange {}])
            .await?;

        self.stream = Some(Stream {
            config: config.clone(),
            decoder: Decoder::new(Bitrate::Mode1_64000, false, false),
        });

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        match self.source.next_event().await? {
            SourceEvent::Frame(frame) => {
                let data = frame.data();

                let samples = Samples::from(stream.decoder.decode(data));

                Ok(SourceEvent::Frame(Frame::new(
                    RawAudioFrame {
                        sample_rate: stream.config.sample_rate,
                        channels: stream.config.channels.clone(),
                        samples,
                    },
                    frame.timestamp,
                )))
            }
            SourceEvent::EndOfData => {
                self.stream = None;
                Ok(SourceEvent::EndOfData)
            }
            SourceEvent::RenegotiationNeeded => {
                self.stream = None;
                Ok(SourceEvent::RenegotiationNeeded)
            }
        }
    }
}
