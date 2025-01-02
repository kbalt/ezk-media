#[derive(Debug, Default, Clone)]
pub struct Options {
    pub offer_transport: TransportType,
    pub rtcp_mux_policy: RtcpMuxPolicy,
    pub bundle_policy: BundlePolicy,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum TransportType {
    Rtp,
    SdesSrtp,
    #[default]
    DtlsSrtp,
}

#[derive(Debug, Default, Clone)]
pub enum RtcpMuxPolicy {
    #[default]
    Negotiate,
    Require,
}

#[derive(Debug, Default, Clone)]
pub enum BundlePolicy {
    Balanced,
    #[default]
    MaxCompat,
    MaxBundle,
}
