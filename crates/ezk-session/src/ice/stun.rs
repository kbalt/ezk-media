use std::{borrow::Cow, net::SocketAddr};
use stun_types::{
    attributes::{
        Fingerprint, IceControlled, IceControlling, MessageIntegrity, MessageIntegrityKey,
        Priority, Username, XorMappedAddress,
    },
    Class, MessageBuilder, Method, TransactionId,
};

use super::{Candidate, IceCredentials};

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
