//! Wire messages. Everything is postcard-encoded; only [`Preamble`] and the
//! SPAKE2 messages travel in plaintext, everything else inside Noise.

use std::net::SocketAddr;

use aura_core::{DeviceId, Entry, EntryId, GroupId, Hlc, Membership};
use serde::{Deserialize, Serialize};

/// First frame on every TCP connection (plaintext, also bound into the Noise prologue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preamble {
    /// A member connecting to sync with another member.
    Sync { proto: u16, group_id: GroupId },
    /// A new device asking to join whatever group the listener belongs to.
    Join { proto: u16 },
}

/// Listener → joiner, plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JoinChallenge {
    Challenge {
        group_id: GroupId,
        group_name: String,
        spake: Vec<u8>,
    },
    Refused {
        reason: String,
    },
}

/// Joiner → listener, plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinSpake {
    pub spake: Vec<u8>,
}

/// Joiner → listener, encrypted (after the PAKE-keyed handshake).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinInfo {
    pub name: String,
    pub listen_port: u16,
}

/// Listener → joiner, encrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JoinResult {
    Welcome(Box<Welcome>),
    Refused(String),
}

/// Version of the group's password key. Rotations are ordered by this; the
/// highest version wins everywhere (the device id breaks HLC ties).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct KeyVersion {
    pub hlc: Hlc,
    pub by: DeviceId,
}

/// A versioned password key `w = Argon2id(passphrase, group_id)`. Debug output is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKey {
    pub version: KeyVersion,
    pub password_key: [u8; 32],
}

impl std::fmt::Debug for GroupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupKey")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// The HTTPS certificate every member uses for the phone page (`aura.local`).
/// Shared so a phone that accepted it once is not warned again when a different
/// PC takes over serving. Debug output is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebCert {
    pub version: KeyVersion,
    pub expires_ms: u64,
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for WebCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebCert")
            .field("version", &self.version)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub group_id: GroupId,
    pub group_name: String,
    /// Current password key, so the new member can admit others too.
    pub key: GroupKey,
    pub web_cert: Option<WebCert>,
    pub membership: Membership,
    /// Known addresses of other members, for networks where mDNS does not work.
    pub addr_hints: Vec<(DeviceId, SocketAddr)>,
}

impl std::fmt::Debug for Welcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Welcome")
            .field("group_id", &self.group_id)
            .field("group_name", &self.group_name)
            .field("members", &self.membership.live_count())
            .finish_non_exhaustive()
    }
}

/// Messages between members over an established sync session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    /// First message each side sends.
    Hello(Hello),
    /// A clipboard entry (new local copy, forwarded flood, or catch-up).
    Entry(Entry),
    /// Membership changed (gossip).
    Membership(Membership),
    /// The group passphrase was changed (gossip; highest version wins).
    GroupKey(GroupKey),
    /// The shared phone-page certificate was created or renewed (gossip; highest version wins).
    WebCert(WebCert),
    /// The sender started or stopped taking part in the phone-page election.
    WebCapable(bool),
    /// Keepalive, so dead connections are noticed after sleep/wake.
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub listen_port: u16,
    pub membership: Membership,
    /// Newest entry the sender has, for catch-up.
    pub head: Option<EntryId>,
    /// Version of the sender's password key, so stale peers get the newer one.
    pub key_version: KeyVersion,
    /// Whether the sender may serve the phone page.
    pub serves_web: bool,
    /// Version of the sender's phone-page certificate, so stale peers get the newer one.
    pub web_cert_version: Option<KeyVersion>,
}
