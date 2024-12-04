use crate::LocalMediaId;
use bytesstr::BytesStr;
use ezk_rtp::RtpPacket;
use tokio::sync::mpsc;

pub struct TransceiverBuilder {
    pub(crate) local_media_id: LocalMediaId,

    pub(crate) m_line_index: usize,
    pub(crate) mid: Option<BytesStr>,
    pub(crate) msid: Option<(BytesStr, BytesStr)>,

    pub(crate) create_receiver: Option<Box<dyn FnMut(mpsc::Receiver<RtpPacket>) + Send + Sync>>,
    pub(crate) create_sender: Option<Box<dyn FnMut(mpsc::Sender<RtpPacket>) + Send + Sync>>,
}

impl TransceiverBuilder {
    /// Id of the media session which uses this transceiver
    pub fn media_id(&self) -> LocalMediaId {
        self.local_media_id
    }

    pub fn m_line_index(&self) -> usize {
        self.m_line_index
    }

    pub fn mid(&self) -> Option<&str> {
        self.mid.as_deref()
    }

    pub fn msid(&self) -> Option<(&str, &str)> {
        self.msid.as_ref().map(|(g, n)| (g.as_str(), n.as_str()))
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
