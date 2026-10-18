//! Runs the real `aura` binary against a test node, while it runs (control
//! channel) and after it stopped (offline search).

use std::process::Command;
use std::time::Duration;

use aura_node::{Config, Content, MemoryClipboard, Node, SecretStoreKind, memory_clipboard};

fn aura(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_aura"))
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("aura runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn aura_search_online_and_offline() {
    let dir = tempfile::tempdir().unwrap();
    let clip = MemoryClipboard::new();
    let cfg = Config {
        device_name: "alpha".into(),
        port: 0,
        discovery: false,
        web_enabled: false,
        poll_interval_ms: 20,
        secret_store: SecretStoreKind::File,
        ..Config::default()
    };
    let node = Node::start(dir.path(), cfg, memory_clipboard(clip.clone()))
        .await
        .unwrap();
    node.create_group("Home", "correct horse battery")
        .await
        .unwrap();
    for text in ["cargo test -p aura-web", "git status", "docker compose up"] {
        node.publish(Content::Text(text.into())).await.unwrap();
    }

    // Running: answered through the control channel.
    let out = aura(dir.path(), &["search", "cgtst"]);
    assert!(out.contains("1. cargo test -p aura-web"), "{out}");
    assert!(out.contains("alpha"), "{out}");
    assert!(!out.contains("isn't running"), "{out}");
    assert!(aura(dir.path(), &["search", "zzzz"]).contains("no matches"));

    // --copy puts the result back on the clipboard (through the running node).
    let out = aura(dir.path(), &["search", "git", "--copy", "1"]);
    assert!(out.contains("copied #1 to every PC"), "{out}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        clip.current(),
        Some(aura_clip::ClipData::Text("git status".into()))
    );

    // Stopped: the CLI reads the saved history itself.
    node.shutdown().await;
    let out = aura(dir.path(), &["search", "dcker"]);
    assert!(out.contains("docker compose up"), "{out}");
    assert!(out.contains("isn't running"), "{out}");
}
