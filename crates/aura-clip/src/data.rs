use std::io::Cursor;

use anyhow::{Context, Result};
use aura_core::Content;

/// Clipboard content as the OS sees it (images as raw RGBA).
#[derive(Clone, PartialEq, Eq)]
pub enum ClipData {
    Text(String),
    Html {
        html: String,
        text: String,
    },
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

impl std::fmt::Debug for ClipData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipData::Text(t) => write!(f, "Text({} chars)", t.chars().count()),
            ClipData::Html { text, .. } => write!(f, "Html({} chars)", text.chars().count()),
            ClipData::Image { width, height, .. } => write!(f, "Image({width}x{height})"),
        }
    }
}

impl ClipData {
    /// Fingerprint used for change detection and echo suppression. HTML is
    /// fingerprinted by its plain text only, because OSes may rewrite the markup
    /// on a round trip.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        match self {
            ClipData::Text(t) | ClipData::Html { text: t, .. } => {
                h.update(b"t");
                h.update(t.as_bytes());
            }
            ClipData::Image {
                width,
                height,
                rgba,
            } => {
                h.update(b"i");
                h.update(&width.to_le_bytes());
                h.update(&height.to_le_bytes());
                h.update(rgba);
            }
        }
        *h.finalize().as_bytes()
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ClipData::Text(t) | ClipData::Html { text: t, .. } => t.is_empty(),
            ClipData::Image { width, height, .. } => *width == 0 || *height == 0,
        }
    }

    /// Converts to the transport representation (images become PNG).
    pub fn to_content(&self) -> Result<Content> {
        Ok(match self {
            ClipData::Text(t) => Content::Text(t.clone()),
            ClipData::Html { html, text } => Content::Html {
                html: html.clone(),
                text: text.clone(),
            },
            ClipData::Image {
                width,
                height,
                rgba,
            } => {
                let img = image::RgbaImage::from_raw(*width, *height, rgba.clone())
                    .context("image buffer size does not match its dimensions")?;
                let mut png = Vec::new();
                img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                    .context("encoding PNG")?;
                Content::ImagePng {
                    width: *width,
                    height: *height,
                    png,
                }
            }
        })
    }

    pub fn from_content(content: &Content) -> Result<Self> {
        Ok(match content {
            Content::Text(t) => ClipData::Text(t.clone()),
            Content::Html { html, text } => ClipData::Html {
                html: html.clone(),
                text: text.clone(),
            },
            Content::ImagePng { png, .. } => {
                let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
                    .context("decoding PNG")?
                    .into_rgba8();
                ClipData::Image {
                    width: img.width(),
                    height: img.height(),
                    rgba: img.into_raw(),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_roundtrip_through_png() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        let data = ClipData::Image {
            width: 3,
            height: 2,
            rgba,
        };
        let content = data.to_content().unwrap();
        assert!(matches!(
            content,
            Content::ImagePng {
                width: 3,
                height: 2,
                ..
            }
        ));
        assert_eq!(ClipData::from_content(&content).unwrap(), data);
    }

    #[test]
    fn html_and_text_share_fingerprint() {
        let a = ClipData::Text("hi".into());
        let b = ClipData::Html {
            html: "<b>hi</b>".into(),
            text: "hi".into(),
        };
        assert_eq!(a.fingerprint(), b.fingerprint());
    }
}
