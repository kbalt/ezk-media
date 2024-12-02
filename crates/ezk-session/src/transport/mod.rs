use std::{future::Future, io};

mod direct;
mod task;

pub(crate) use direct::DirectRtpTransport;
pub(crate) use task::{IdentifyableBy, TransportTaskHandle};

pub trait RtpTransport: Send + 'static {
    fn recv(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn send_rtp(&mut self, buf: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn send_rtcp(&mut self, buf: &[u8]) -> impl Future<Output = io::Result<()>> + Send {
        self.send_rtp(buf)
    }
}
