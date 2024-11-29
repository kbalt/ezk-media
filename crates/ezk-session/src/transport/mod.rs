use ezk::{Frame, NextEventIsCancelSafe, Source, SourceEvent, ValueRange};
use ezk_rtp::{Rtp, RtpConfig, RtpConfigRange, RtpPacket};
use tokio::sync::mpsc;

mod direct;

pub(crate) use direct::DirectRtpTransport;

pub(crate) enum MediaTransport {
    Direct(DirectRtpTransport),
}

pub(super) struct RtpMpscSource {
    pub(super) rx: mpsc::Receiver<RtpPacket>,
    pub(super) pt: u8,
}

impl Source for RtpMpscSource {
    type MediaType = Rtp;

    async fn capabilities(&mut self) -> ezk::Result<Vec<RtpConfigRange>> {
        Ok(vec![RtpConfigRange {
            pt: ValueRange::Value(self.pt),
        }])
    }

    async fn negotiate_config(&mut self, available: Vec<RtpConfigRange>) -> ezk::Result<RtpConfig> {
        assert!(available.iter().any(|r| r.pt.contains(&self.pt)));
        Ok(RtpConfig { pt: self.pt })
    }

    async fn next_event(&mut self) -> ezk::Result<SourceEvent<Rtp>> {
        match self.rx.recv().await {
            Some(packet) => Ok(SourceEvent::Frame(Frame::new(packet, 0))),
            None => Ok(SourceEvent::EndOfData),
        }
    }
}

impl NextEventIsCancelSafe for RtpMpscSource {}
