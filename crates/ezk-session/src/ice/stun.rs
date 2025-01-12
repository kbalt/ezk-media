use std::{borrow::Cow, cmp::min, net::SocketAddr, time::Duration};
use stun_types::{
    attributes::{
        Fingerprint, IceControlled, IceControlling, MessageIntegrity, MessageIntegrityKey,
        Priority, Username, XorMappedAddress,
    },
    Class, Message, MessageBuilder, Method, TransactionId,
};

use super::{Candidate, IceCredentials};

pub(crate) struct StunConfig {
    pub(crate) initial_rto: Duration,
    pub(crate) max_retransmits: u32,
    pub(crate) max_rto: Duration,
}

impl StunConfig {
    pub(crate) fn new() -> Self {
        Self {
            // Copying str0m & libwebrtc defaults here
            initial_rto: Duration::from_millis(250),
            // RFC 5389 default
            max_retransmits: 7,
            // Like str0m & libwebrtc capping the maximum retransmit value
            max_rto: Duration::from_secs(8),
        }
    }

    pub(crate) fn retransmit_delta(&self, attempts: u32) -> Duration {
        let rto = Duration::from_millis(
            (self.initial_rto.as_millis() << attempts)
                .try_into()
                .unwrap(),
        );

        min(rto, self.max_rto)
    }
}

pub(super) fn make_binding_request(
    transaction_id: TransactionId,
    local_credentials: &IceCredentials,
    remote_credentials: &IceCredentials,
    local_candidate: &Candidate,
    is_controlling: bool,
    control_tie_breaker: u64,
) -> Vec<u8> {
    let mut stun_message = MessageBuilder::new(Class::Request, Method::Binding, transaction_id);

    let username = format!("{}:{}", remote_credentials.ufrag, local_credentials.ufrag);
    stun_message.add_attr(&Username::new(&username)).unwrap();
    stun_message
        .add_attr(&Priority(local_candidate.priority))
        .unwrap();

    if is_controlling {
        stun_message
            .add_attr(&IceControlling(control_tie_breaker))
            .unwrap();
    } else {
        stun_message
            .add_attr(&IceControlled(control_tie_breaker))
            .unwrap();
    }

    stun_message
        .add_attr_with(
            &MessageIntegrity::default(),
            &MessageIntegrityKey::new_raw(Cow::Borrowed(remote_credentials.pwd.as_bytes())),
        )
        .unwrap();

    stun_message.add_attr(&Fingerprint).unwrap();

    stun_message.finish()
}

pub(super) fn make_success_response(
    transaction_id: TransactionId,
    local_credentials: &IceCredentials,
    remote_credentials: &IceCredentials,
    source: SocketAddr,
) -> Vec<u8> {
    let mut stun_message = MessageBuilder::new(Class::Success, Method::Binding, transaction_id);

    let username = format!("{}:{}", local_credentials.ufrag, remote_credentials.ufrag);

    stun_message.add_attr(&Username::new(&username)).unwrap();
    stun_message.add_attr(&XorMappedAddress(source)).unwrap();
    stun_message
        .add_attr_with(
            &MessageIntegrity::default(),
            &MessageIntegrityKey::new_raw(Cow::Borrowed(remote_credentials.pwd.as_bytes())),
        )
        .unwrap();

    stun_message.add_attr(&Fingerprint).unwrap();

    stun_message.finish()
}

pub(crate) fn verify_integrity(
    local_credentials: &IceCredentials,
    remote_credentials: &IceCredentials,
    stun_msg: &mut Message,
) -> bool {
    let is_request = match stun_msg.class() {
        Class::Request | Class::Indication => true,
        Class::Success | Class::Error => false,
    };

    let key = if is_request {
        &local_credentials.pwd
    } else {
        &remote_credentials.pwd
    };

    let passed_integrity_check = stun_msg
        .attribute_with::<MessageIntegrity>(&MessageIntegrityKey::new_raw(Cow::Borrowed(
            key.as_bytes(),
        )))
        .is_some_and(|r| r.is_ok());

    let expected_username = if is_request {
        format!("{}:{}", local_credentials.ufrag, remote_credentials.ufrag)
    } else {
        format!("{}:{}", remote_credentials.ufrag, local_credentials.ufrag)
    };

    let username = stun_msg.attribute::<Username>().unwrap().unwrap();

    passed_integrity_check && username.0 == expected_username
}
