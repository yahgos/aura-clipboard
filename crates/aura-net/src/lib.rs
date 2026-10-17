//! Networking for Aura Clipboard.
//!
//! - [`session`]: Noise `XX` sessions over TCP with message chunking.
//! - [`enroll`]: joining a group with the passphrase (SPAKE2 + Noise `XXpsk3`).
//! - [`discovery`]: mDNS advertisement and browsing (`_aura._tcp.local.`).
//! - [`proto`]: the messages exchanged over those channels.

pub mod discovery;
pub mod enroll;
pub mod identity;
pub mod proto;
pub mod session;

pub use identity::Identity;
pub use session::{Incoming, Session, SessionReader, SessionWriter};
