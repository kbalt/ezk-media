use bitflags::bitflags;
use ezk::ValueRange;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channels {
    NotPositioned(u32),
    Positioned(Vec<ChannelPosition>),
}

impl Channels {
    #[must_use]
    pub fn channel_count(&self) -> usize {
        match self {
            Self::NotPositioned(count) => {
                usize::try_from(*count).expect("not supporting more than usize::MAX channels")
            }
            Self::Positioned(positions) => positions.len(),
        }
    }

    #[must_use]
    pub fn any() -> ValueRange<Self> {
        ValueRange::range(
            Self::NotPositioned(1),
            Self::NotPositioned(u32::from(u16::MAX)),
        )
    }

    #[must_use]
    pub fn is_positioned(&self) -> bool {
        matches!(self, Self::Positioned(..))
    }

    #[must_use]
    pub fn is_mono(&self) -> bool {
        match self {
            Self::NotPositioned(1) => true,
            Self::NotPositioned(_) => false,
            Self::Positioned(positions) => {
                matches!(&positions[..], [ChannelPosition::MONO])
            }
        }
    }

    #[must_use]
    pub fn is_stereo(&self) -> bool {
        match self {
            Self::NotPositioned(2) => true,
            Self::NotPositioned(_) => false,
            Self::Positioned(positions) => {
                matches!(
                    &positions[..],
                    [ChannelPosition::FRONT_LEFT, ChannelPosition::FRONT_RIGHT]
                )
            }
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct ChannelPosition: u32 {
        const MONO = 0x00000000;
        const FRONT_LEFT = 0x00000001;
        const FRONT_RIGHT = 0x00000002;
        const FRONT_CENTER = 0x00000004;
        const LOW_FREQUENCY = 0x00000008;
        const BACK_LEFT = 0x00000010;
        const BACK_RIGHT = 0x00000020;
        const FRONT_LEFT_OF_CENTER = 0x00000040;
        const FRONT_RIGHT_OF_CENTER = 0x00000080;
        const BACK_CENTER = 0x00000100;
        const SIDE_LEFT = 0x00000200;
        const SIDE_RIGHT = 0x00000400;
        const TOP_CENTER = 0x00000800;
        const TOP_FRONT_LEFT = 0x00001000;
        const TOP_FRONT_CENTER = 0x00002000;
        const TOP_FRONT_RIGHT = 0x00004000;
        const TOP_BACK_LEFT = 0x00008000;
        const TOP_BACK_CENTER = 0x00010000;
        const TOP_BACK_RIGHT = 0x00020000;
    }
}
