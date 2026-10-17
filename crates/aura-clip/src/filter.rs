use anyhow::Result;

use crate::{ClipData, ClipboardBackend};

/// Turns clipboard polling into "the user copied something new" events.
///
/// Echo suppression: after the engine writes a remote entry, the filter records
/// both the OS change token and the content fingerprint, so the write is not
/// reported back as a local copy (which would ping-pong between PCs).
#[derive(Debug, Default)]
pub struct ChangeFilter {
    last_token: Option<u64>,
    last_fingerprint: Option<[u8; 32]>,
}

impl ChangeFilter {
    /// Records whatever is on the clipboard at startup without reporting it.
    pub fn prime(backend: &mut dyn ClipboardBackend) -> Self {
        let mut f = Self {
            last_token: backend.change_token(),
            last_fingerprint: None,
        };
        if !backend.is_concealed()
            && let Ok(Some(data)) = backend.read()
        {
            f.last_fingerprint = Some(data.fingerprint());
        }
        f
    }

    /// Returns new local content, if any.
    pub fn poll(&mut self, backend: &mut dyn ClipboardBackend) -> Result<Option<ClipData>> {
        let token = backend.change_token();
        if token.is_some() && token == self.last_token {
            return Ok(None);
        }
        self.last_token = token;
        if backend.is_concealed() {
            self.last_fingerprint = None;
            return Ok(None);
        }
        let Some(data) = backend.read()? else {
            return Ok(None);
        };
        if data.is_empty() {
            return Ok(None);
        }
        let fp = data.fingerprint();
        if self.last_fingerprint == Some(fp) {
            return Ok(None);
        }
        self.last_fingerprint = Some(fp);
        Ok(Some(data))
    }

    /// Writes remote content and makes sure it is not reported as a local change.
    pub fn write(&mut self, backend: &mut dyn ClipboardBackend, data: &ClipData) -> Result<()> {
        backend.write(data)?;
        self.last_fingerprint = Some(data.fingerprint());
        self.last_token = backend.change_token();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryClipboard;

    fn text(s: &str) -> ClipData {
        ClipData::Text(s.into())
    }

    #[test]
    fn reports_user_copies_once_and_ignores_startup_content() {
        let user = MemoryClipboard::new();
        user.user_copy(text("before start"));
        let mut b = user.clone();
        let mut f = ChangeFilter::prime(&mut b);
        assert_eq!(f.poll(&mut b).unwrap(), None);

        user.user_copy(text("hello"));
        assert_eq!(f.poll(&mut b).unwrap(), Some(text("hello")));
        assert_eq!(f.poll(&mut b).unwrap(), None);

        // Copying the same thing again is not a change.
        user.user_copy(text("hello"));
        assert_eq!(f.poll(&mut b).unwrap(), None);
    }

    #[test]
    fn own_writes_are_not_echoed() {
        let mut b = MemoryClipboard::new();
        let mut f = ChangeFilter::prime(&mut b);
        f.write(&mut b, &text("from peer")).unwrap();
        assert_eq!(f.poll(&mut b).unwrap(), None);
        assert_eq!(b.current(), Some(text("from peer")));
    }

    #[test]
    fn concealed_content_is_skipped() {
        let user = MemoryClipboard::new();
        let mut b = user.clone();
        let mut f = ChangeFilter::prime(&mut b);
        user.user_copy_concealed(text("hunter2"));
        assert_eq!(f.poll(&mut b).unwrap(), None);
        user.user_copy(text("hunter2"));
        assert_eq!(f.poll(&mut b).unwrap(), Some(text("hunter2")));
    }
}
