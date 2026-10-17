use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use aura_core::Content;

use crate::{ChangeFilter, ClipData, ClipboardBackend};

/// Owns the clipboard on a dedicated thread: polls for local copies and applies
/// remote writes. PNG encoding/decoding also happens on that thread so callers
/// never block on images. Dropping the worker stops the thread.
///
/// Each local copy is reported with the number of writes the worker had applied
/// before reading it. If the caller has issued more writes than that, a remote
/// write may have overwritten the user's copy on the OS clipboard.
pub struct ClipboardWorker {
    write_tx: mpsc::Sender<Content>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ClipboardWorker {
    pub fn spawn<B, F>(
        make_backend: B,
        poll: Duration,
        mut on_local_copy: F,
    ) -> anyhow::Result<Self>
    where
        B: FnOnce() -> anyhow::Result<Box<dyn ClipboardBackend>> + Send + 'static,
        F: FnMut(Content, u64) + Send + 'static,
    {
        let (write_tx, write_rx) = mpsc::channel::<Content>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("aura-clipboard".into())
            .spawn(move || {
                // Some platforms (X11) want the clipboard created on the thread that uses it.
                let mut backend = match make_backend() {
                    Ok(b) => {
                        let _ = ready_tx.send(Ok(()));
                        b
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut filter = ChangeFilter::prime(backend.as_mut());
                let mut writes_applied = 0u64;
                loop {
                    match write_rx.recv_timeout(poll) {
                        Ok(content) => {
                            writes_applied += 1;
                            let res = ClipData::from_content(&content)
                                .and_then(|data| filter.write(backend.as_mut(), &data));
                            if let Err(e) = res {
                                tracing::warn!("writing clipboard failed: {e:#}");
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => match filter.poll(backend.as_mut()) {
                            Ok(Some(data)) => match data.to_content() {
                                Ok(content) => on_local_copy(content, writes_applied),
                                Err(e) => {
                                    tracing::warn!("converting clipboard content failed: {e:#}")
                                }
                            },
                            Ok(None) => {}
                            Err(e) => tracing::debug!("reading clipboard failed: {e:#}"),
                        },
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        ready_rx.recv()??;
        Ok(Self {
            write_tx,
            thread: Some(thread),
        })
    }

    /// Puts remote content on the local clipboard.
    pub fn write(&self, content: Content) {
        let _ = self.write_tx.send(content);
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        // Replace the sender so the thread sees a disconnect, then join it.
        let (dead_tx, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.write_tx, dead_tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryClipboard;
    use std::sync::mpsc::channel;

    #[test]
    fn worker_reports_and_writes() {
        let user = MemoryClipboard::new();
        let backend = user.clone();
        let (tx, rx) = channel();
        let worker = ClipboardWorker::spawn(
            move || Ok(Box::new(backend) as Box<dyn ClipboardBackend>),
            Duration::from_millis(5),
            move |d, _| tx.send(d).unwrap(),
        )
        .unwrap();
        user.user_copy(ClipData::Text("a".into()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Content::Text("a".into())
        );
        worker.write(Content::Text("remote".into()));
        let remote = Some(ClipData::Text("remote".into()));
        for _ in 0..200 {
            if user.current() == remote {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(user.current(), remote);
        // Give the worker a few poll cycles to (wrongly) report the write back.
        thread::sleep(Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "remote write must not echo");
    }
}
