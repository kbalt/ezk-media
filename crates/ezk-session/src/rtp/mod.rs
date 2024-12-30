use ezk_rtp::rtp_types::{prelude::RtpPacketWriter, RtpPacketBuilder, RtpPacketMut};
use std::marker::PhantomData;

mod extensions;

pub(crate) use extensions::{RtpExtensionIds, RtpExtensions};

#[derive(Default, Debug)]
pub(crate) struct RtpPacketWriterVec<'a> {
    output: Vec<u8>,
    padding: Option<u8>,
    phantom: PhantomData<&'a [u8]>,
}

impl<'a> RtpPacketWriter for RtpPacketWriterVec<'a> {
    type Output = Vec<u8>;
    type Payload = &'a [u8];
    type Extension = Vec<u8>;

    fn reserve(&mut self, size: usize) {
        if self.output.len() < size {
            self.output.reserve(size - self.output.len());
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.output.extend_from_slice(data)
    }

    fn push_extension(&mut self, extension_data: &Self::Extension) {
        self.push(extension_data)
    }

    fn push_payload(&mut self, data: &Self::Payload) {
        self.push(data)
    }

    fn padding(&mut self, size: u8) {
        self.padding = Some(size);
    }

    fn finish(&mut self) -> Self::Output {
        let mut ret = vec![];
        if let Some(padding) = self.padding.take() {
            self.output
                .resize(self.output.len() + padding as usize - 1, 0);
            self.output.push(padding);
        }
        std::mem::swap(&mut ret, &mut self.output);
        ret
    }
}

pub(crate) fn to_builder<'a>(
    packet_mut: &'a RtpPacketMut<'a>,
) -> RtpPacketBuilder<&'a [u8], Vec<u8>> {
    let mut builder = RtpPacketBuilder::new()
        .marker_bit(packet_mut.marker_bit())
        .payload_type(packet_mut.payload_type())
        .sequence_number(packet_mut.sequence_number())
        .timestamp(packet_mut.timestamp())
        .ssrc(packet_mut.ssrc())
        .payload(packet_mut.payload());

    if let Some(padding) = packet_mut.padding() {
        builder = builder.padding(padding);
    }

    builder
}
