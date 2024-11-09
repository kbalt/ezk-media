use bytes::Bytes;
use ezk::{ConfigRange, Frame, MediaType};
use ezk_rtp::{DePayloader, Payloadable, Payloader};
use std::{iter::from_fn, mem::take};

pub mod libg722;

mod decoder;
mod encoder;

pub use decoder::G722Decoder;
pub use encoder::G722Encoder;

#[derive(Debug)]
pub enum G722 {}

impl MediaType for G722 {
    type ConfigRange = G722ConfigRange;
    type Config = G722Config;
    type FrameData = Bytes;
}

#[derive(Debug, Clone)]
pub struct G722ConfigRange;

impl ConfigRange for G722ConfigRange {
    type Config = G722Config;

    fn any() -> Self {
        Self {}
    }

    fn intersect(&self, _other: &Self) -> Option<Self> {
        Some(Self {})
    }

    fn contains(&self, _config: &Self::Config) -> bool {
        true
    }
}

#[derive(Default, Debug, Clone)]
pub struct G722Config;

impl Payloadable for G722 {
    type Payloader = G722Payloader;
    type DePayloader = G722DePayloader;

    const STATIC_PT: Option<u8> = Some(9);

    fn make_payloader(_: Self::Config) -> Self::Payloader {
        G722Payloader {}
    }

    fn make_depayloader(_: Vec<Self::ConfigRange>) -> (Self::Config, Self::DePayloader) {
        (Self::Config {}, G722DePayloader {})
    }
}

pub struct G722Payloader;

impl Payloader<G722> for G722Payloader {
    fn payload(&mut self, frame: Frame<G722>, max_size: usize) -> impl Iterator<Item = Bytes> + '_ {
        let mut data = frame.into_data();

        from_fn(move || {
            if let Some((pkg, rem)) = data.split_at_checked(max_size) {
                let pkg = data.slice_ref(pkg);
                data = data.slice_ref(rem);
                Some(pkg)
            } else if data.is_empty() {
                None
            } else {
                Some(take(&mut data))
            }
        })
    }
}

pub struct G722DePayloader;

impl DePayloader<G722> for G722DePayloader {
    fn depayload(&mut self, payload: &[u8]) -> Bytes {
        Bytes::copy_from_slice(payload)
    }
}
