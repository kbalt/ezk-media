use crate::LocalMediaId;
use ezk_rtp::RtpPacket;
use tokio::sync::mpsc;

pub struct TransceiverBuilder {
    pub(crate) local_media_id: LocalMediaId,

    pub(crate) create_receiver: Option<Box<dyn FnMut(mpsc::Receiver<RtpPacket>) + Send + Sync>>,
    pub(crate) create_sender: Option<Box<dyn FnMut(mpsc::Sender<RtpPacket>) + Send + Sync>>,
}

impl TransceiverBuilder {
    /// Id of the media session which uses this transceiver
    pub fn media_id(&self) -> LocalMediaId {
        self.local_media_id
    }

    pub fn add_receiver<F>(&mut self, on_create: F)
    where
        F: FnMut(mpsc::Receiver<RtpPacket>) + Send + Sync + 'static,
    {
        self.create_receiver = Some(Box::new(on_create));
    }

    pub fn add_sender<F>(&mut self, on_create: F)
    where
        F: FnMut(mpsc::Sender<RtpPacket>) + Send + Sync + 'static,
    {
        self.create_sender = Some(Box::new(on_create));
    }
}
