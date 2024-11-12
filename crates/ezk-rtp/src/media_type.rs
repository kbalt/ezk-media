use crate::RtpPacket;
use ezk::{ConfigRange, MediaType, ValueRange};

/// RTP Payload marker type
#[derive(Debug)]
pub enum Rtp {}

impl MediaType for Rtp {
    type ConfigRange = RtpConfigRange;
    type Config = RtpConfig;
    type FrameData = RtpPacket;
}

#[derive(Debug, Clone)]
pub struct RtpConfigRange {
    pub pt: ValueRange<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct RtpConfig {
    pub pt: u8,
}

impl ConfigRange for RtpConfigRange {
    type Config = RtpConfig;

    fn any() -> Self {
        RtpConfigRange {
            pt: ValueRange::AnyOf(vec![ValueRange::range(0, 71), ValueRange::range(96, 127)]),
        }
    }

    fn intersect(&self, other: &Self) -> Option<Self> {
        Some(Self {
            pt: self.pt.intersect(&other.pt)?,
        })
    }

    fn contains(&self, config: &Self::Config) -> bool {
        let Self { pt } = self;
        pt.contains(&config.pt)
    }
}
