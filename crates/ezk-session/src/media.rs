use std::time::Instant;

use ezk_rtp::RtpSession;

pub struct Media {
    rtp_session: RtpSession,
    next_report: Instant,
}
