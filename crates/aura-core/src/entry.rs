use serde::{Deserialize, Serialize};

use crate::{DeviceId, Hlc};

/// Globally unique and totally ordered entry id. The greatest id is the
/// shared "current" clipboard (last writer wins, device id breaks ties).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntryId {
    pub hlc: Hlc,
    pub origin: DeviceId,
}

impl EntryId {
    /// Big-endian bytes that sort the same way as `EntryId` itself (handy as a DB key).
    pub fn to_key(&self) -> [u8; 28] {
        let mut k = [0u8; 28];
        k[..8].copy_from_slice(&self.hlc.wall_ms.to_be_bytes());
        k[8..12].copy_from_slice(&self.hlc.counter.to_be_bytes());
        k[12..].copy_from_slice(&self.origin.0);
        k
    }

    pub fn from_key(k: &[u8]) -> Option<Self> {
        if k.len() != 28 {
            return None;
        }
        let mut origin = [0u8; 16];
        origin.copy_from_slice(&k[12..]);
        Some(Self {
            hlc: Hlc {
                wall_ms: u64::from_be_bytes(k[..8].try_into().ok()?),
                counter: u32::from_be_bytes(k[8..12].try_into().ok()?),
            },
            origin: DeviceId(origin),
        })
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.to_key())
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        Self::from_key(&hex::decode(s).ok()?)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    /// Rich text with its plain-text alternative.
    Html {
        html: String,
        text: String,
    },
    /// Image encoded as PNG for transport.
    ImagePng {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
}

impl Content {
    pub fn kind(&self) -> &'static str {
        match self {
            Content::Text(_) => "text",
            Content::Html { .. } => "html",
            Content::ImagePng { .. } => "image",
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Content::Text(t) => t.len(),
            Content::Html { html, text } => html.len() + text.len(),
            Content::ImagePng { png, .. } => png.len(),
        }
    }

    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(self.kind().as_bytes());
        match self {
            Content::Text(t) => {
                h.update(t.as_bytes());
            }
            Content::Html { html, text } => {
                h.update(&(html.len() as u64).to_le_bytes());
                h.update(html.as_bytes());
                h.update(text.as_bytes());
            }
            Content::ImagePng { width, height, png } => {
                h.update(&width.to_le_bytes());
                h.update(&height.to_le_bytes());
                h.update(png);
            }
        }
        *h.finalize().as_bytes()
    }

    /// One-line human preview for menus and logs.
    pub fn preview(&self, max_chars: usize) -> String {
        let text = match self {
            Content::Text(t) | Content::Html { text: t, .. } => t,
            Content::ImagePng { width, height, png } => {
                return format!("[image {width}×{height}, {} KB]", png.len().div_ceil(1024));
            }
        };
        let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if one_line.chars().count() > max_chars {
            let cut: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
            format!("{cut}…")
        } else {
            one_line
        }
    }
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({:?})", self.kind(), self.preview(32))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub content: Content,
}

impl Entry {
    pub fn meta(&self) -> EntryMeta {
        EntryMeta {
            id: self.id,
            kind: self.content.kind().to_string(),
            preview: self.content.preview(60),
            size: self.content.size(),
        }
    }
}

/// Lightweight description of an entry for UIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMeta {
    pub id: EntryId,
    pub kind: String,
    pub preview: String,
    pub size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order_matches_id_order() {
        let a = EntryId {
            hlc: Hlc {
                wall_ms: 5,
                counter: 9,
            },
            origin: DeviceId([9; 16]),
        };
        let b = EntryId {
            hlc: Hlc {
                wall_ms: 6,
                counter: 0,
            },
            origin: DeviceId([0; 16]),
        };
        let c = EntryId {
            hlc: Hlc {
                wall_ms: 6,
                counter: 0,
            },
            origin: DeviceId([1; 16]),
        };
        assert!(a < b && b < c);
        assert!(a.to_key() < b.to_key() && b.to_key() < c.to_key());
        assert_eq!(EntryId::from_hex(&c.to_hex()), Some(c));
    }

    #[test]
    fn preview_truncates_and_flattens() {
        let c = Content::Text("hello\n   world, this is long".into());
        assert_eq!(c.preview(11), "hello worl…");
        assert_eq!(c.preview(100), "hello world, this is long");
    }

    #[test]
    fn hash_distinguishes_kinds() {
        let t = Content::Text("x".into());
        let h = Content::Html {
            html: "".into(),
            text: "x".into(),
        };
        assert_ne!(t.hash(), h.hash());
    }
}
