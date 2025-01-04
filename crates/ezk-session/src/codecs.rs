use sdp_types::MediaType;
use std::borrow::Cow;

// TODO: allow ulpfec https://www.rfc-editor.org/rfc/rfc5109?

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Codec {
    /// Either set by the codec itself if it's static, or assigned later when added to a session
    pub(crate) pt: Option<u8>,
    pub(crate) name: Cow<'static, str>,
    pub(crate) clock_rate: u32,
    pub(crate) channels: Option<u32>,
    pub(crate) params: Vec<String>,
}

impl Codec {
    pub const PCMU: Self = Self::new("PCMU", 8000).with_static_pt(0);
    pub const PCMA: Self = Self::new("PCMA", 8000).with_static_pt(8);
    pub const G722: Self = Self::new("G722", 8000).with_static_pt(9).with_channels(1);
    pub const OPUS: Self = Self::new("OPUS", 48_000).with_channels(2);

    pub const H264: Self = Self::new("H264", 90_000);
    pub const VP8: Self = Self::new("VP8", 90_000);
    pub const VP9: Self = Self::new("VP9", 90_000);
    pub const AV1: Self = Self::new("AV1", 90_000);

    pub const fn new(name: &'static str, clock_rate: u32) -> Self {
        Self {
            pt: None,
            name: Cow::Borrowed(name),
            clock_rate,
            channels: None,
            params: vec![],
        }
    }

    pub const fn with_static_pt(mut self, static_pt: u8) -> Self {
        self.pt = Some(static_pt);
        self
    }

    pub const fn with_channels(mut self, channels: u32) -> Self {
        self.channels = Some(channels);
        self
    }

    pub fn with_param(mut self, param: impl Into<String>) {
        self.params.push(param.into());
    }
}

pub struct Codecs {
    pub(crate) media_type: MediaType,
    pub(crate) codecs: Vec<Codec>,
    pub(crate) allow_rtx: bool,
    pub(crate) allow_dtmf: bool,
}

impl Codecs {
    pub fn new(media_type: MediaType) -> Self {
        Self {
            media_type,
            codecs: vec![],
            allow_rtx: false,
            allow_dtmf: false,
        }
    }

    pub fn allow_rtx(mut self, rtx: bool) -> Self {
        self.allow_rtx = rtx;
        self
    }

    pub fn allow_dtmf(mut self, dtmf: bool) -> Self {
        self.allow_dtmf = dtmf;
        self
    }

    pub fn with_codec(mut self, codec: Codec) -> Self {
        self.add_codec(codec);
        self
    }

    pub fn add_codec(&mut self, codec: Codec) -> &mut Self {
        self.codecs.push(codec);
        self
    }
}
