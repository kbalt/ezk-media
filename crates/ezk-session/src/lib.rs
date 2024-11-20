use ezk::BoxedSourceCancelSafe;
use ezk_rtp::Rtp;
use sdp_session::LocalMediaId;
use sdp_types::MediaType;

mod rtp_session;
mod sdp_session;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Codec {
    pub static_pt: Option<u8>,
    pub name: String,
    pub clock_rate: u32,
    pub params: Vec<String>,
}

pub struct Codecs {
    media_type: MediaType,
    codecs: Vec<CodecsEntry>,
}

struct CodecsEntry {
    codec: Codec,
    build: Box<dyn FnMut(&mut TransceiverBuilder)>,
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
        F: FnMut(&mut TransceiverBuilder) + 'static,
    {
        self.add_codec(codec, on_use);
        self
    }

    pub fn add_codec<F>(&mut self, codec: Codec, on_use: F) -> &mut Self
    where
        F: FnMut(&mut TransceiverBuilder) + 'static,
    {
        self.codecs.push(CodecsEntry {
            codec,
            build: Box::new(on_use),
        });

        self
    }
}

pub struct TransceiverBuilder {
    media_id: LocalMediaId,
    create_receive: Option<Box<dyn FnMut(BoxedSourceCancelSafe<Rtp>)>>,
    create_sender: Option<Box<dyn FnMut() -> BoxedSourceCancelSafe<Rtp>>>,
}

impl TransceiverBuilder {
    /// Id of the media session which uses this transceiver
    pub fn media_id(&self) -> LocalMediaId {
        self.media_id
    }

    pub fn add_receiver<F>(&mut self, on_create: F)
    where
        F: FnMut(BoxedSourceCancelSafe<Rtp>) + Send + 'static,
    {
        self.create_receive = Some(Box::new(on_create));
    }

    pub fn add_sender<F>(&mut self, on_create: F)
    where
        F: FnMut() -> BoxedSourceCancelSafe<Rtp> + Send + 'static,
    {
        self.create_sender = Some(Box::new(on_create));
    }
}

#[test]
fn ye() {
    use sdp_session::Session;
    use std::net::{IpAddr, Ipv4Addr};
    let mut session = Session::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));

    let codecs = Codecs::new(MediaType::Audio).with_codec(
        Codec {
            static_pt: Some(9),
            name: "G722".into(),
            clock_rate: 8000,
            params: vec![],
        },
        |transceiver| {
            transceiver.add_receiver(|_source| {});
            transceiver.add_sender(|| todo!());
        },
    );

    session.add_local_media(codecs, 1);
}
