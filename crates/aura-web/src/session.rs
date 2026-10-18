//! Stateless tokens for the phone page.
//!
//! A token is `expiry ‖ kind ‖ tag`, where `tag` is a BLAKE3 keyed hash under the
//! group's web session key. Every member derives the same key from the group
//! password key, so a token issued by one PC is valid on whichever PC serves the
//! page next, and changing the passphrase invalidates every token.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const SESSION_COOKIE: &str = "aura_session";
pub const SESSION_TTL_SECS: u64 = 30 * 24 * 60 * 60;
pub const QR_TTL_SECS: u64 = 10 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Cookie for a logged-in phone.
    Session = 1,
    /// Short-lived token in the QR code shown on a PC; exchanged for a session.
    QrLogin = 2,
}

const TAG_LEN: usize = 16;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn tag(key: &[u8; 32], body: &[u8]) -> [u8; TAG_LEN] {
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&blake3::keyed_hash(key, body).as_bytes()[..TAG_LEN]);
    out
}

pub fn issue(key: &[u8; 32], kind: Kind, expires_secs: u64) -> String {
    let mut bytes = Vec::with_capacity(9 + TAG_LEN);
    bytes.extend_from_slice(&expires_secs.to_be_bytes());
    bytes.push(kind as u8);
    let t = tag(key, &bytes);
    bytes.extend_from_slice(&t);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Returns the token's expiry if it is genuine, of the expected kind and not expired.
pub fn verify(key: &[u8; 32], kind: Kind, token: &str, now_secs: u64) -> Option<u64> {
    let bytes = URL_SAFE_NO_PAD.decode(token.trim()).ok()?;
    if bytes.len() != 9 + TAG_LEN || bytes[8] != kind as u8 {
        return None;
    }
    let (body, given) = bytes.split_at(9);
    let expected = tag(key, body);
    let diff = expected
        .iter()
        .zip(given)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    let expires = u64::from_be_bytes(body[..8].try_into().ok()?);
    (diff == 0 && expires > now_secs).then_some(expires)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_verify_only_with_the_right_key_kind_and_time() {
        let key = [7u8; 32];
        let t = issue(&key, Kind::Session, 1_000);
        assert_eq!(verify(&key, Kind::Session, &t, 999), Some(1_000));
        assert_eq!(verify(&key, Kind::Session, &t, 1_000), None, "expired");
        assert_eq!(verify(&[8u8; 32], Kind::Session, &t, 0), None, "other key");
        assert_eq!(verify(&key, Kind::QrLogin, &t, 0), None, "other kind");
        let mut forged = URL_SAFE_NO_PAD.decode(&t).unwrap();
        forged[7] ^= 1; // extend the expiry
        assert_eq!(
            verify(&key, Kind::Session, &URL_SAFE_NO_PAD.encode(forged), 0),
            None
        );
        assert_eq!(verify(&key, Kind::Session, "garbage", 0), None);
    }
}
