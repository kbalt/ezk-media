use crate::TransceiverBuilder;
use sdp_types::MediaType;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Codec {
    pub static_pt: Option<u8>,
    pub name: String,
    pub clock_rate: u32,
    pub params: Vec<String>,
}

pub struct Codecs {
    pub(crate) media_type: MediaType,
    pub(crate) codecs: Vec<CodecsEntry>,
}

pub(crate) struct CodecsEntry {
    pub(crate) codec: Codec,
    pub(crate) build: Box<dyn FnMut(&mut TransceiverBuilder) + Send + Sync>,
}

impl Codecs {
    pub fn new(media_type: MediaType) -> Self {
        Self {
            media_type,
            codecs: vec![],
        }
    }

    pub fn with_codec<F>(mut self, codec: Codec, on_use: F) -> Self
    where
        F: FnMut(&mut TransceiverBuilder) + Send + Sync + 'static,
    {
        self.add_codec(codec, on_use);
        self
    }

    pub fn add_codec<F>(&mut self, codec: Codec, on_use: F) -> &mut Self
    where
        F: FnMut(&mut TransceiverBuilder) + Send + Sync + 'static,
    {
        self.codecs.push(CodecsEntry {
            codec,
            build: Box::new(on_use),
        });

        self
    }
}
