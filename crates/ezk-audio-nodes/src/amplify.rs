use ezk::{NextEventIsCancelSafe, Result, Source, SourceEvent};
use ezk_audio::{match_samples, RawAudio, RawAudioConfig, RawAudioConfigRange, Sample};

pub struct Amplify<S> {
    source: S,
    amp: f32,
}

impl<S: Source<MediaType = RawAudio> + NextEventIsCancelSafe> NextEventIsCancelSafe for Amplify<S> {}

impl<S: Source<MediaType = RawAudio>> Amplify<S> {
    pub fn new(source: S, amp: f32) -> Self {
        Self { source, amp }
    }
}

impl<S: Source<MediaType = RawAudio>> Source for Amplify<S> {
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        self.source.capabilities().await
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        self.source.negotiate_config(available).await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        match self.source.next_event().await? {
            SourceEvent::Frame(mut frame) => {
                let data = frame.make_data_mut();

                match_samples!((&mut data.samples) => (samples) => amp(samples, self.amp));

                Ok(SourceEvent::Frame(frame))
            }
            SourceEvent::EndOfData => Ok(SourceEvent::EndOfData),
            SourceEvent::RenegotiationNeeded => Ok(SourceEvent::RenegotiationNeeded),
        }
    }
}

fn amp<S>(samples: &mut [S], mul: f32)
where
    S: Sample,
{
    for sample in samples.iter_mut() {
        *sample = sample.saturating_mul_f64(f64::from(mul));
    }
}
