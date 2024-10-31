use bytes::Bytes;
use ezk::{ConfigRange, Frame, MediaType};
use ezk_rtp::{DePayloader, Payloadable, Payloader};

pub mod alaw;
pub mod mulaw;

mod decoder;
mod encoder;

pub use decoder::G711Decoder;
pub use encoder::G711Encoder;

pub type PCMUDecoder<S> = decoder::G711Decoder<S, PCMU>;
pub type PCMADecoder<S> = decoder::G711Decoder<S, PCMA>;

pub type PCMUEncoder<S> = encoder::G711Encoder<S, PCMU>;
pub type PCMAEncoder<S> = encoder::G711Encoder<S, PCMA>;

macro_rules! pcmx {
    ($n:ident, $cr:ident, $c:ident, $pt:expr) => {
        #[derive(Debug)]
        pub enum $n {}

        impl MediaType for $n {
            type ConfigRange = PCMAConfigRange;
            type Config = PCMAConfig;
            type FrameData = Bytes;
        }

        #[derive(Debug, Clone)]
        pub struct $cr;

        impl ConfigRange for $cr {
            type Config = $c;

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
        pub struct $c;

        impl Payloadable for $n {
            type Payloader = G711Payloader;
            type DePayloader = G711DePayloader;

            const STATIC_PT: Option<u8> = Some($pt);

            fn make_payloader(_: Self::Config) -> Self::Payloader {
                G711Payloader {}
            }

            fn make_depayloader(_: Vec<Self::ConfigRange>) -> (Self::Config, Self::DePayloader) {
                (Self::Config {}, G711DePayloader {})
            }
        }
    };
}

pcmx!(PCMU, PCMUConfigRange, PCMUConfig, 0);
pcmx!(PCMA, PCMAConfigRange, PCMAConfig, 8);

pub trait PCMX: MediaType<Config: Default, FrameData = Bytes> {
    fn encode(i: &[i16]) -> Vec<u8>;
    fn decode(i: &[u8]) -> Vec<i16>;
}

impl PCMX for PCMA {
    fn encode(i: &[i16]) -> Vec<u8> {
        i.iter().copied().map(alaw::encode).collect()
    }

    fn decode(i: &[u8]) -> Vec<i16> {
        i.iter().copied().map(alaw::decode).collect()
    }
}

impl PCMX for PCMU {
    fn encode(i: &[i16]) -> Vec<u8> {
        i.iter().copied().map(mulaw::encode).collect()
    }

    fn decode(i: &[u8]) -> Vec<i16> {
        i.iter().copied().map(mulaw::decode).collect()
    }
}

pub struct G711Payloader;

impl Payloader<PCMU> for G711Payloader {
    fn payload(&mut self, frame: Frame<PCMU>) -> impl Iterator<Item = Bytes> + '_ {
        Some(frame.into_data()).into_iter()
    }
}

impl Payloader<PCMA> for G711Payloader {
    fn payload(&mut self, frame: Frame<PCMA>) -> impl Iterator<Item = Bytes> + '_ {
        Some(frame.into_data()).into_iter()
    }
}

pub struct G711DePayloader;

impl DePayloader<PCMU> for G711DePayloader {
    fn depayload(&mut self, payload: &[u8]) -> Bytes {
        Bytes::copy_from_slice(payload)
    }
}

impl DePayloader<PCMA> for G711DePayloader {
    fn depayload(&mut self, payload: &[u8]) -> Bytes {
        Bytes::copy_from_slice(payload)
    }
}
