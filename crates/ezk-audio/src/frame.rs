use crate::{Channels, SampleRate, Samples};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RawAudioFrame {
    pub sample_rate: SampleRate,
    pub channels: Channels,
    pub samples: Samples,
}

impl RawAudioFrame {
    #[must_use]
    pub fn duration(&self) -> Duration {
        let samples_per_channel = u64::try_from(self.samples.len() / self.channels.channel_count())
            .expect("samples per channel should be less than u64::MAX");

        let nanos = 1_000_000_000 * samples_per_channel / u64::from(self.sample_rate.0);

        Duration::from_nanos(nanos)
    }
}
