use ezk::{Error, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent, ValueRange};
use ezk_audio::{
    Channels, Format, RawAudio, RawAudioConfig, RawAudioConfigRange, RawAudioFrame, SampleRate,
    Samples, SamplesQueue,
};
use nnnoiseless::DenoiseState;

pub struct NoiseFilter<S> {
    source: S,
    stream: Option<Stream>,
}

struct Stream {
    first: bool,
    queue: SamplesQueue,
    state: Box<DenoiseState<'static>>,
}

impl<S: Source<MediaType = RawAudio> + NextEventIsCancelSafe> NextEventIsCancelSafe
    for NoiseFilter<S>
{
}

impl<S: Source<MediaType = RawAudio>> NoiseFilter<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            stream: None,
        }
    }
}

impl<S: Source<MediaType = RawAudio>> Source for NoiseFilter<S> {
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        let cap = self.source.capabilities().await?;

        if cap.iter().any(|r| {
            r.sample_rate.contains(&SampleRate(48000))
                && r.channels.contains(&Channels::NotPositioned(1))
                && r.format.contains(&Format::I16)
        }) {
            Ok(vec![RawAudioConfigRange {
                sample_rate: ValueRange::Value(SampleRate(48000)),
                channels: ValueRange::Value(Channels::NotPositioned(1)),
                format: ValueRange::Value(Format::I16),
            }])
        } else {
            Err(Error::msg(
                "NoiseFilter failed to find matching capabilities from upstream",
            ))
        }
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        if !available.iter().any(|r| {
            r.sample_rate.contains(&SampleRate(48000))
                && r.channels.contains(&Channels::NotPositioned(1))
                && r.format.contains(&Format::I16)
        }) {
            return Err(Error::msg(
                "NoiseFilter failed to find matching capabilities from upstream",
            ));
        }

        self.stream = Some(Stream {
            first: true,
            queue: SamplesQueue::empty(Format::I16),
            state: DenoiseState::new(),
        });

        self.source
            .negotiate_config(vec![RawAudioConfigRange {
                sample_rate: ValueRange::Value(SampleRate(48000)),
                channels: ValueRange::Value(Channels::NotPositioned(1)),
                format: ValueRange::Value(Format::I16),
            }])
            .await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(stream) = &mut self.stream else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        loop {
            if let Some(samples) = stream.queue.pop_exact(DenoiseState::FRAME_SIZE) {
                let Samples::I16(samples) = &samples else {
                    panic!()
                };

                // TODO: reuse buffers here
                let input: Vec<f32> = samples.iter().map(|&i| i as f32).collect();
                let mut output: Vec<f32> = vec![0.0; input.len()];

                stream.state.process_frame(&mut output, &input);

                if stream.first {
                    stream.first = false;
                    continue;
                }

                return Ok(SourceEvent::Frame(Frame::new(
                    RawAudioFrame {
                        sample_rate: SampleRate(48000),
                        channels: Channels::NotPositioned(1),
                        samples: Samples::from(Vec::from_iter(
                            output.into_iter().map(|i| i as i16),
                        )),
                    },
                    0,
                )));
            }

            match self.source.next_event().await? {
                SourceEvent::Frame(frame) => {
                    stream.queue.extend(&frame.data().samples);
                }
                SourceEvent::EndOfData => return Ok(SourceEvent::EndOfData),
                SourceEvent::RenegotiationNeeded => return Ok(SourceEvent::RenegotiationNeeded),
            }
        }
    }
}
