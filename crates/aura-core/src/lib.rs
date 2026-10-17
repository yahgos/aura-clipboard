//! Pure, I/O-free data model for Aura Clipboard.
//!
//! Everything in here is deterministic so it can be property-tested: hybrid
//! logical clocks, clipboard entries with last-writer-wins ordering, the
//! membership 2P-set CRDT and the bounded history.

pub mod entry;
pub mod history;
pub mod hlc;
pub mod id;
pub mod membership;

pub use entry::{Content, Entry, EntryId, EntryMeta};
pub use history::{History, InsertOutcome};
pub use hlc::{Hlc, HlcClock};
pub use id::{DeviceId, GroupId};
pub use membership::{Member, Membership};

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTOCOL_VERSION: u16 = 1;
