use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::{ClipData, platform};

/// Abstraction over a clipboard. Methods are blocking and called from a single
/// dedicated thread.
pub trait ClipboardBackend: Send {
    /// Cheap counter that changes whenever the clipboard changes, if the OS has one.
    /// `None` means the caller must compare content to detect changes.
    fn change_token(&mut self) -> Option<u64>;

    /// True if the current content is flagged by its source app (e.g. a password
    /// manager) as something that must not be synced or kept in history.
    fn is_concealed(&mut self) -> bool;

    /// Reads the current content, or `None` if empty / unsupported format.
    fn read(&mut self) -> Result<Option<ClipData>>;

    fn write(&mut self, data: &ClipData) -> Result<()>;
}

/// The real OS clipboard (arboard for content, native APIs for change tokens).
pub struct SystemClipboard {
    inner: arboard::Clipboard,
    /// Platforms without a change token poll images less often (reading them is expensive).
    tick: u64,
}

impl SystemClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: arboard::Clipboard::new()?,
            tick: 0,
        })
    }
}

impl ClipboardBackend for SystemClipboard {
    fn change_token(&mut self) -> Option<u64> {
        platform::change_token()
    }

    fn is_concealed(&mut self) -> bool {
        platform::is_concealed()
    }

    fn read(&mut self) -> Result<Option<ClipData>> {
        self.tick += 1;
        if let Ok(text) = self.inner.get_text() {
            if text.is_empty() {
                return Ok(None);
            }
            return Ok(Some(match self.inner.get().html() {
                Ok(html) if !html.is_empty() => ClipData::Html { html, text },
                _ => ClipData::Text(text),
            }));
        }
        // Without a change token we poll; only look at images every 8th tick (~2 s).
        if (platform::HAS_CHANGE_TOKEN || self.tick.is_multiple_of(8))
            && let Ok(img) = self.inner.get_image()
        {
            return Ok(Some(ClipData::Image {
                width: img.width as u32,
                height: img.height as u32,
                rgba: img.bytes.into_owned(),
            }));
        }
        Ok(None)
    }

    fn write(&mut self, data: &ClipData) -> Result<()> {
        match data {
            ClipData::Text(t) => self.inner.set_text(t.as_str())?,
            ClipData::Html { html, text } => {
                self.inner.set_html(html.as_str(), Some(text.as_str()))?
            }
            ClipData::Image {
                width,
                height,
                rgba,
            } => self.inner.set_image(arboard::ImageData {
                width: *width as usize,
                height: *height as usize,
                bytes: Cow::Borrowed(rgba),
            })?,
        }
        Ok(())
    }
}

/// In-memory clipboard for tests and headless setups. Clones share state, so a
/// test can keep one handle to simulate the user copying things.
#[derive(Clone, Default)]
pub struct MemoryClipboard {
    state: Arc<Mutex<MemState>>,
}

#[derive(Default)]
struct MemState {
    data: Option<ClipData>,
    counter: u64,
    concealed: bool,
    writes: u64,
}

impl MemoryClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates a user copy in another app.
    pub fn user_copy(&self, data: ClipData) {
        let mut s = self.state.lock().unwrap();
        s.data = Some(data);
        s.counter += 1;
        s.concealed = false;
    }

    /// Simulates a password manager copying a secret.
    pub fn user_copy_concealed(&self, data: ClipData) {
        self.user_copy(data);
        self.state.lock().unwrap().concealed = true;
    }

    pub fn current(&self) -> Option<ClipData> {
        self.state.lock().unwrap().data.clone()
    }

    /// Number of writes performed by the engine (to detect echo loops in tests).
    pub fn engine_writes(&self) -> u64 {
        self.state.lock().unwrap().writes
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn change_token(&mut self) -> Option<u64> {
        Some(self.state.lock().unwrap().counter)
    }

    fn is_concealed(&mut self) -> bool {
        self.state.lock().unwrap().concealed
    }

    fn read(&mut self) -> Result<Option<ClipData>> {
        Ok(self.state.lock().unwrap().data.clone())
    }

    fn write(&mut self, data: &ClipData) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.data = Some(data.clone());
        s.counter += 1;
        s.concealed = false;
        s.writes += 1;
        Ok(())
    }
}
