use crate::LocalMediaId;
use ezk::BoxedSourceCancelSafe;
use ezk_rtp::Rtp;

pub struct TransceiverBuilder {
    pub(crate) local_media_id: LocalMediaId,

    pub(crate) create_receiver: Option<Box<dyn FnMut(BoxedSourceCancelSafe<Rtp>) + Send + Sync>>,
    pub(crate) create_sender: Option<Box<dyn FnMut() -> BoxedSourceCancelSafe<Rtp> + Send + Sync>>,
}

impl TransceiverBuilder {
    /// Id of the media session which uses this transceiver
    pub fn media_id(&self) -> LocalMediaId {
        self.local_media_id
    }

    pub fn add_receiver<F>(&mut self, on_create: F)
    where
        F: FnMut(BoxedSourceCancelSafe<Rtp>) + Send + Sync + 'static,
    {
        self.create_receiver = Some(Box::new(on_create));
    }

    pub fn add_sender<F>(&mut self, on_create: F)
    where
        F: FnMut() -> BoxedSourceCancelSafe<Rtp> + Send + Sync + 'static,
    {
        self.create_sender = Some(Box::new(on_create));
    }
}
