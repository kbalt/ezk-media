use crate::{parse_extensions, RtpExtensionsWriter, RtpTimestamp, SequenceNumber, Ssrc};
use bytes::Bytes;
use rtp_types::{prelude::RtpPacketWriter, RtpPacketBuilder};

#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub pt: u8,
    pub sequence_number: SequenceNumber,
    pub ssrc: Ssrc,
    pub timestamp: RtpTimestamp,
    pub extensions: RtpExtensions,
    pub payload: Bytes,
}

#[derive(Debug, Default, Clone)]
pub struct RtpExtensions {
    pub mid: Option<Bytes>,
}

/// ID to attribute type map to use when parsing or serializing RTP packets
#[derive(Debug, Default, Clone, Copy)]
pub struct RtpExtensionIds {
    pub mid: Option<u8>,
}

impl RtpPacket {
    pub fn write_vec(&self, extension_ids: RtpExtensionIds, vec: &mut Vec<u8>) {
        let builder = RtpPacketBuilder::<_, Vec<u8>>::new()
            .payload_type(self.pt)
            .sequence_number(self.sequence_number.0)
            .ssrc(self.ssrc.0)
            .timestamp(self.timestamp.0)
            .payload(&self.payload[..]);

        let builder = self.extensions.write(extension_ids, builder);

        vec.reserve(builder.calculate_size().unwrap());

        let mut writer = RtpPacketWriterVec {
            output: vec,
            padding: None,
        };
        builder.write(&mut writer).unwrap();
    }

    pub fn to_vec(&self, extension_ids: RtpExtensionIds) -> Vec<u8> {
        let mut vec = vec![];
        self.write_vec(extension_ids, &mut vec);
        vec
    }

    pub fn parse(
        extension_ids: RtpExtensionIds,
        bytes: impl Into<Bytes>,
    ) -> Result<Self, rtp_types::RtpParseError> {
        let packet: Bytes = bytes.into();

        let parsed = rtp_types::RtpPacket::parse(&packet[..])?;

        let extensions = if let Some((profile, extension_data)) = parsed.extension() {
            RtpExtensions::from_packet(extension_ids, &packet, profile, extension_data)
        } else {
            RtpExtensions { mid: None }
        };

        Ok(Self {
            pt: parsed.payload_type(),
            sequence_number: SequenceNumber(parsed.sequence_number()),
            ssrc: Ssrc(parsed.ssrc()),
            timestamp: RtpTimestamp(parsed.timestamp()),
            extensions,
            payload: packet.slice_ref(parsed.payload()),
        })
    }
}

impl RtpExtensions {
    fn from_packet(
        ids: RtpExtensionIds,
        bytes: &Bytes,
        profile: u16,
        extension_data: &[u8],
    ) -> Self {
        let mut this = Self { mid: None };

        for (id, data) in parse_extensions(profile, extension_data) {
            if Some(id) == ids.mid {
                this.mid = Some(bytes.slice_ref(data));
            }
        }

        this
    }

    fn write<'b>(
        &self,
        ids: RtpExtensionIds,
        packet_builder: RtpPacketBuilder<&'b [u8], Vec<u8>>,
    ) -> RtpPacketBuilder<&'b [u8], Vec<u8>> {
        let Some((id, mid)) = ids.mid.zip(self.mid.as_ref()) else {
            return packet_builder;
        };

        let mut buf = vec![];

        let profile = RtpExtensionsWriter::new(&mut buf, mid.len() <= 16)
            .with(id, mid)
            .finish();

        packet_builder.extension(profile, buf)
    }
}

struct RtpPacketWriterVec<'a> {
    output: &'a mut Vec<u8>,
    padding: Option<u8>,
}

impl<'a> RtpPacketWriter for RtpPacketWriterVec<'a> {
    type Output = ();
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
        if let Some(padding) = self.padding.take() {
            self.output
                .resize(self.output.len() + padding as usize - 1, 0);
            self.output.push(padding);
        }
    }
}

enum ExtensionsIter<T, U> {
    OneByte(T),
    TwoBytes(U),
    None,
}

impl<T: Iterator, U: Iterator<Item = T::Item>> Iterator for ExtensionsIter<T, U> {
    type Item = T::Item;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ExtensionsIter::OneByte(iter) => iter.next(),
            ExtensionsIter::TwoBytes(iter) => iter.next(),
            ExtensionsIter::None => None,
        }
    }
}

pub fn parse_extensions(profile: u16, data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    if dbg!(profile) == 0xBEDE {
        ExtensionsIter::OneByte(parse_onebyte(data))
    } else if (profile & 0xFFF) == 0x100 {
        ExtensionsIter::TwoBytes(parse_twobyte(data))
    } else {
        ExtensionsIter::None
    }
}

// https://www.rfc-editor.org/rfc/rfc5285#section-4.2
fn parse_onebyte(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let [b, remaining @ ..] = data else {
            return None;
        };

        dbg!(b);

        let id = (b & 0xF0) >> 4;
        if id == 15 {
            return None;
        }

        let len = (b & 0x0F) as usize + 1;
        let padding = padding_32_bit_boundry(2 + len);

        if remaining.len() >= len {
            data = &remaining[len + padding..];
            Some((id, &remaining[..len]))
        } else {
            None
        }
    })
}

// https://www.rfc-editor.org/rfc/rfc5285#section-4.3
fn parse_twobyte(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let [id, len, remaining @ ..] = data else {
            return None;
        };

        let len = *len as usize;
        let padding = padding_32_bit_boundry(len);

        if remaining.len() >= len {
            data = &remaining[len + padding..];
            Some((*id, &remaining[..len]))
        } else {
            None
        }
    })
}

fn padding_32_bit_boundry(i: usize) -> usize {
    match i % 4 {
        0 => 0,
        1 => 3,
        2 => 2,
        3 => 1,
        _ => unreachable!(),
    }
}
