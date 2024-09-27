use crate::{Channels, Format, SampleRate};
use ezk::{ConfigRange, ValueRange};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RawAudioConfigRange {
    pub sample_rate: ValueRange<SampleRate>,
    pub channels: ValueRange<Channels>,
    pub format: ValueRange<Format>,
}

impl ConfigRange for RawAudioConfigRange {
    type Config = RawAudioConfig;

    fn any() -> Self {
        Self {
            sample_rate: SampleRate::any(),
            channels: Channels::any(),
            format: Format::all(),
        }
    }

    fn intersect(&self, other: &Self) -> Option<Self> {
        Some(Self {
            sample_rate: self.sample_rate.intersect(&other.sample_rate)?,
            channels: self.channels.intersect(&other.channels)?,
            format: self.format.intersect(&other.format)?,
        })
    }

    fn contains(&self, config: &Self::Config) -> bool {
        let Self {
            sample_rate,
            channels,
            format,
        } = self;

        sample_rate.contains(&config.sample_rate)
            && channels.contains(&config.channels)
            && format.contains(&config.format)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RawAudioConfig {
    pub sample_rate: SampleRate,
    pub channels: Channels,
    pub format: Format,
}
