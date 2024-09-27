use self::channels::ChannelMixer;
use self::format::convert_sample_format;
use self::rate::RateConverter;
use ezk::{ConfigRange, NextEventIsCancelSafe, Result, Source, SourceEvent};
use ezk_audio::{RawAudio, RawAudioConfig, RawAudioConfigRange};

mod channels;
mod format;
mod rate;

/// Converts any [`RawAudio`] to match downstream's requirements
pub struct AudioConvert<S> {
    source: S,

    stream: Option<Stream>,
}

impl<S: Source<MediaType = RawAudio> + NextEventIsCancelSafe> NextEventIsCancelSafe
    for AudioConvert<S>
{
}

struct Stream {
    config: RawAudioConfig,
    channel_mixer: Option<ChannelMixer>,
    rate_converter: Option<RateConverter>,
}

impl<S: Source<MediaType = RawAudio>> AudioConvert<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            stream: None,
        }
    }
}

impl<S: Source<MediaType = RawAudio>> Source for AudioConvert<S> {
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        let mut caps = self.source.capabilities().await?;
        caps.push(RawAudioConfigRange::any());
        Ok(caps)
    }

    async fn negotiate_config(
        &mut self,
        mut available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        // Keep a copy of the original offer, to find out later if the negotiated config is valid with downstream or not
        let mut original = available.clone();

        available.push(RawAudioConfigRange::any());

        let negotiated_config = self.source.negotiate_config(available).await?;

        // Find out if converting is required or the config can just passed through
        if original
            .iter()
            .any(|original| original.contains(&negotiated_config))
        {
            // Easy path, no converting required
            self.stream = Some(Stream {
                config: negotiated_config.clone(),
                channel_mixer: None,
                rate_converter: None,
            });

            return Ok(negotiated_config);
        }

        // Hard path, set up converter
        // TODO: find a config requiring the least amount of conversion
        let best_config = original.remove(0);

        let best_config = dbg!(RawAudioConfig {
            sample_rate: best_config.sample_rate.first_value(),
            channels: best_config.channels.first_value(),
            format: best_config.format.first_value(),
        });

        let channel_mixer = if negotiated_config.channels != best_config.channels {
            Some(ChannelMixer::new(
                negotiated_config.channels.clone(),
                best_config.channels.clone(),
            ))
        } else {
            None
        };

        let rate_converter = if negotiated_config.sample_rate != best_config.sample_rate {
            Some(RateConverter::new(
                best_config.format,
                negotiated_config.sample_rate,
                best_config.sample_rate,
                best_config.channels.clone(),
            ))
        } else {
            None
        };
        println!("rate: {}", rate_converter.is_some());
        println!("channel: {}", channel_mixer.is_some());

        self.stream = Some(Stream {
            config: best_config.clone(),
            channel_mixer,
            rate_converter,
        });

        Ok(best_config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        loop {
            match self.source.next_event().await? {
                SourceEvent::Frame(mut frame) => {
                    if let Some(channel_mixer) = &mut stream.channel_mixer {
                        frame = channel_mixer.convert(frame);
                    }

                    if let Some(rate_converter) = stream.rate_converter.as_mut() {
                        // RateConverter also converts the sample type
                        if let Some(f) = rate_converter.convert(frame) {
                            frame = f;
                        } else {
                            // Frame was consumed into the rate-converter's internal queue
                            continue;
                        }
                    } else {
                        frame = convert_sample_format(frame, stream.config.format);
                    }

                    return Ok(SourceEvent::Frame(frame));
                }
                SourceEvent::EndOfData => return Ok(SourceEvent::EndOfData),
                SourceEvent::RenegotiationNeeded => return Ok(SourceEvent::RenegotiationNeeded),
            }
        }
    }
}