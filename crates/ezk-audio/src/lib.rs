#![warn(unreachable_pub)]

#[cfg(target_endian = "big")]
compile_error!(
    "\t(╯°□°)╯︵ ┻━┻
Big endian is currently not supported!
Please create an issue if you need it."
);

use ezk::MediaType;

#[macro_use]
mod sample_format;
mod channels;
mod config;
mod frame;
mod sample_rate;
mod sample_types;

pub use channels::{ChannelPosition, Channels};
pub use config::{RawAudioConfig, RawAudioConfigRange};
pub use frame::RawAudioFrame;
pub use sample_format::{Format, Samples, SamplesQueue};
pub use sample_rate::SampleRate;
pub use sample_types::{Sample, I24, U24};

#[doc(hidden)]
pub mod __private_macro_exports {
    pub use crate::sample_types::{I24, U24};
    pub use {f32, f64};
    pub use {i16, i32, i64, i8};
    pub use {u16, u32, u64, u8};
}

#[derive(Debug)]
pub enum RawAudio {}

impl MediaType for RawAudio {
    type ConfigRange = RawAudioConfigRange;
    type Config = RawAudioConfig;
    type FrameData = RawAudioFrame;
}
