use core::fmt;

/// Owned wrapper around [`rtp_types::RtpPacket`]
#[derive(Clone)]
pub struct RtpPacket(Vec<u8>);

impl fmt::Debug for RtpPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

impl RtpPacket {
    pub fn new(packet: &rtp_types::RtpPacketBuilder<&[u8], &[u8]>) -> Self {
        Self(packet.write_vec_unchecked())
    }

    pub fn parse(i: &[u8]) -> Result<Self, rtp_types::RtpParseError> {
        let _packet = rtp_types::RtpPacket::parse(i)?;

        Ok(Self(i.to_vec()))
    }

    pub fn get(&self) -> rtp_types::RtpPacket<'_> {
        rtp_types::RtpPacket::parse(&self.0)
            .expect("internal buffer must contain a valid rtp packet")
    }

    pub fn get_mut(&mut self) -> rtp_types::RtpPacketMut<'_> {
        rtp_types::RtpPacketMut::parse(&mut self.0[..])
            .expect("internal buffer must contain a valid rtp packet")
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
    if profile == 0xBEDE {
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

        let id = b & 0x0F;
        let len = ((b & 0xF0) >> 4) as usize;
        let padding = padding_32_bit_boundry(1 + len);

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
        let padding = padding_32_bit_boundry(2 + len);

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
