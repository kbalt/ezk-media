use std::net::{IpAddr, Ipv4Addr};

use ezk::BoxedSourceCancelSafe;
use ezk_rtp::Rtp;
use sdp_types::{Connection, Direction, IceOptions, MediaType, Origin, SessionDescription, Time};

pub struct Session {
    sdp_id: u64,
    sdp_version: u64,

    address: IpAddr,
}

struct MediaSession {}

impl Session {
    pub fn new(address: IpAddr) -> Self {
        Self {
            sdp_id: rand::random(),
            sdp_version: rand::random(),
            address,
        }
    }

    pub fn create_sdp(&self) -> SessionDescription {
        SessionDescription {
            name: "-".into(),
            origin: Origin {
                username: "-".into(),
                session_id: self.sdp_id.to_string().into(),
                session_version: self.sdp_version.to_string().into(),
                address: self.address.into(),
            },
            time: Time { start: 0, stop: 0 },
            direction: Direction::SendRecv,
            connection: Some(Connection {
                address: self.address.into(),
                ttl: None,
                num: None,
            }),
            bandwidth: vec![],
            ice_options: IceOptions::default(),
            ice_lite: false,
            ice_ufrag: None,
            ice_pwd: None,
            attributes: vec![],
            media_descriptions: vec![],
        }
    }

    pub fn add_media_session(&mut self, codecs: Codecs) {}
}

pub struct Codecs {
    media_type: MediaType,
    codecs: Vec<CodecsEntry>,
}

struct CodecsEntry {
    pt: Option<u8>,

    build: Box<dyn FnMut(&mut TransceiverBuilder)>,
}

impl Codecs {
    pub fn new(media_type: MediaType) -> Self {
        Self {
            media_type,
            codecs: vec![],
        }
    }

    pub fn with_codec<F: FnMut(&mut TransceiverBuilder)>(
        mut self,
        static_pt: Option<u8>,
        encoding_name: &str,
        clock_rate: u32,
        on_use: F,
    ) -> Self {
        self.add_codec(static_pt, encoding_name, clock_rate, on_use);
        self
    }

    pub fn add_codec<F: FnMut(&mut TransceiverBuilder)>(
        &mut self,
        static_pt: Option<u8>,
        encoding_name: &str,
        clock_rate: u32,
        on_use: F,
    ) -> &mut Self {
        self
    }
}

pub struct TransceiverBuilder {
    rx: Option<Box<dyn FnOnce(BoxedSourceCancelSafe<Rtp>)>>,
    tx: Option<Box<dyn FnOnce() -> BoxedSourceCancelSafe<Rtp>>>,
}

impl TransceiverBuilder {
    pub fn add_receiver<F>(&mut self, on_create: F)
    where
        F: FnOnce(BoxedSourceCancelSafe<Rtp>) + Send + 'static,
    {
        self.rx = Some(Box::new(on_create));
    }

    pub fn add_sender<F>(&mut self, on_create: F)
    where
        F: FnOnce() -> BoxedSourceCancelSafe<Rtp> + Send + 'static,
    {
        self.tx = Some(Box::new(on_create));
    }
}

#[test]
fn ye() {
    let mut session = Session::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));

    let codecs = Codecs::new(MediaType::Audio).with_codec(Some(9), "G722", 8000, |transceiver| {
        transceiver.add_receiver(|source| {});
        transceiver.add_sender(|| todo!());
    });

    session.add_media_session(codecs);
}
