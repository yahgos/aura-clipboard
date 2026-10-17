//! Needs an X server and `xclip`: `DISPLAY=:99 cargo test -p aura-clip -- --ignored`.
#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Command, Stdio};
use std::time::Duration;

use aura_clip::{ClipboardBackend, SystemClipboard};

/// Puts `text` on the X clipboard, offered only under `target`.
fn xclip(text: &str, target: &str) {
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", target])
        .stdin(Stdio::piped())
        // xclip forks a daemon that owns the selection; don't let it hold our pipes.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("xclip installed");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
#[ignore = "needs an X server and xclip"]
fn kde_password_hint_is_concealed() {
    let mut clip = SystemClipboard::new().unwrap();

    xclip("hello", "UTF8_STRING");
    assert!(!clip.is_concealed());
    assert!(clip.read().unwrap().is_some());

    xclip("secret", "x-kde-passwordManagerHint");
    assert!(clip.is_concealed());

    xclip("hello again", "UTF8_STRING");
    assert!(!clip.is_concealed());
}
