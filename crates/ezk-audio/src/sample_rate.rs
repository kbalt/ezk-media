use ezk::ValueRange;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SampleRate(pub u32);

impl SampleRate {
    /// Calculate the duration for the given amount of samples (per channel)
    #[must_use]
    pub fn duration_for_samples(self, len: usize) -> Duration {
        Duration::from_nanos((1_000_000_000 * len as u64) / u64::from(self.0))
    }

    /// List of common sample rates encountered in the wild
    #[must_use]
    pub const fn common() -> &'static [SampleRate] {
        &COMMON_SAMPLE_RATES
    }

    #[must_use]
    pub fn any() -> ValueRange<Self> {
        ValueRange::AnyOf(vec![
            // Add some good rates which should be picked when choosing an arbitrary sample rate
            ValueRange::Value(SampleRate(48000)),
            ValueRange::Value(SampleRate(44100)),
            ValueRange::Value(SampleRate(32000)),
            ValueRange::Value(SampleRate(16000)),
            ValueRange::Value(SampleRate(8000)),
            ValueRange::range(
                *COMMON_SAMPLE_RATES.first().unwrap(),
                *COMMON_SAMPLE_RATES.last().unwrap(),
            ),
        ])
    }
}

const COMMON_SAMPLE_RATES: [SampleRate; 15] = [
    SampleRate(5512),
    SampleRate(8000),
    SampleRate(11025),
    SampleRate(16000),
    SampleRate(22050),
    SampleRate(32000),
    SampleRate(44100),
    SampleRate(48000),
    SampleRate(64000),
    SampleRate(88200),
    SampleRate(96000),
    SampleRate(176_400),
    SampleRate(192_000),
    SampleRate(352_800),
    SampleRate(384_000),
];
