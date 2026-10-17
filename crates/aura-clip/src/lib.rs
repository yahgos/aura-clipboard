//! OS clipboard access for Aura Clipboard.
//!
//! - [`ClipboardBackend`] abstracts the OS clipboard ([`SystemClipboard`]) so the
//!   engine can be tested with [`MemoryClipboard`].
//! - [`ChangeFilter`] turns polling into change events and suppresses echoes of
//!   entries we wrote ourselves.
//! - [`ClipboardWorker`] runs both on a dedicated thread.

mod backend;
mod data;
mod filter;
mod platform;
mod worker;

pub use backend::{ClipboardBackend, MemoryClipboard, SystemClipboard};
pub use data::ClipData;
pub use filter::ChangeFilter;
pub use worker::ClipboardWorker;
