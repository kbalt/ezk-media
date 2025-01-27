use sdp_types::TransportProtocol;

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub offer_transport: TransportType,
    pub offer_ice: bool,
    pub offer_avpf: bool,
    pub rtcp_mux_policy: RtcpMuxPolicy,
    pub bundle_policy: BundlePolicy,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransportType {
    Rtp,
    SdesSrtp,
    #[default]
    DtlsSrtp,
}

impl TransportType {
    pub(crate) fn sdp_type(&self, avpf: bool) -> TransportProtocol {
        if avpf {
            match self {
                Self::Rtp => TransportProtocol::RtpAvpf,
                Self::SdesSrtp => TransportProtocol::RtpSavpf,
                Self::DtlsSrtp => TransportProtocol::UdpTlsRtpSavpf,
            }
        } else {
            match self {
                Self::Rtp => TransportProtocol::RtpAvp,
                Self::SdesSrtp => TransportProtocol::RtpSavp,
                Self::DtlsSrtp => TransportProtocol::UdpTlsRtpSavp,
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RtcpMuxPolicy {
    #[default]
    Negotiate,
    Require,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BundlePolicy {
    // TODO: does Balanced really need to be a thing?
    // Balanced,
    #[default]
    MaxCompat,
    MaxBundle,
}
