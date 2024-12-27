use base64::{prelude::BASE64_STANDARD, Engine};
use rand::RngCore;
use sdp_types::{
    SrtpCrypto, SrtpKeyingMaterial,
    SrtpSuite::{self, *},
};
use srtp::CryptoPolicy;
use std::io;

pub(super) fn negotiate_sdes_srtp(
    remote_crypto: &[SrtpCrypto],
) -> io::Result<(Vec<SrtpCrypto>, srtp::Session, srtp::Session)> {
    let choice1 = remote_crypto
        .iter()
        .find(|c| c.suite == AES_256_CM_HMAC_SHA1_80 && !c.keys.is_empty());
    let choice2 = remote_crypto
        .iter()
        .find(|c| c.suite == AES_256_CM_HMAC_SHA1_32 && !c.keys.is_empty());
    let choice3 = remote_crypto
        .iter()
        .find(|c| c.suite == AES_CM_128_HMAC_SHA1_80 && !c.keys.is_empty());
    let choice4 = remote_crypto
        .iter()
        .find(|c| c.suite == AES_CM_128_HMAC_SHA1_32 && !c.keys.is_empty());

    let crypto = choice1
        .or(choice2)
        .or(choice3)
        .or(choice4)
        .ok_or_else(|| io::Error::other("No compatible srtp suite found"))?;

    let recv_key = BASE64_STANDARD
        .decode(&crypto.keys[0].key_and_salt)
        .map_err(io::Error::other)?;

    let suite = srtp_suite_to_policy(&crypto.suite).unwrap();

    let mut send_key = vec![0u8; suite.key_len()];
    rand::thread_rng().fill_bytes(&mut send_key);

    let inbound = srtp::Session::with_inbound_template(srtp::StreamPolicy {
        rtp: suite,
        rtcp: suite,
        key: &recv_key,
        ..Default::default()
    })
    .unwrap();

    let outbound = srtp::Session::with_outbound_template(srtp::StreamPolicy {
        rtp: suite,
        rtcp: suite,
        key: &send_key,
        ..Default::default()
    })
    .unwrap();

    Ok((
        vec![SrtpCrypto {
            tag: crypto.tag,
            suite: crypto.suite.clone(),
            keys: vec![SrtpKeyingMaterial {
                key_and_salt: BASE64_STANDARD.encode(&send_key).into(),
                lifetime: None,
                mki: None,
            }],
            params: vec![],
        }],
        inbound,
        outbound,
    ))
}

fn srtp_suite_to_policy(suite: &SrtpSuite) -> Option<CryptoPolicy> {
    match suite {
        SrtpSuite::AES_CM_128_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_128_hmac_sha1_80()),
        SrtpSuite::AES_CM_128_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_128_hmac_sha1_32()),
        SrtpSuite::AES_192_CM_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_192_hmac_sha1_80()),
        SrtpSuite::AES_192_CM_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_192_hmac_sha1_32()),
        SrtpSuite::AES_256_CM_HMAC_SHA1_80 => Some(CryptoPolicy::aes_cm_256_hmac_sha1_80()),
        SrtpSuite::AES_256_CM_HMAC_SHA1_32 => Some(CryptoPolicy::aes_cm_256_hmac_sha1_32()),
        SrtpSuite::AEAD_AES_128_GCM => Some(CryptoPolicy::aes_gcm_128_16_auth()),
        SrtpSuite::AEAD_AES_256_GCM => Some(CryptoPolicy::aes_gcm_256_16_auth()),
        _ => None,
    }
}
