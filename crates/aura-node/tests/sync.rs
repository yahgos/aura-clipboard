//! End-to-end tests: several nodes in one process, talking over localhost with
//! in-memory clipboards.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use aura_clip::ClipData;
use aura_node::{
    Config, JoinTarget, MemoryClipboard, Node, NodeHandle, SecretStoreKind, memory_clipboard,
};
use tempfile::TempDir;

const PASS: &str = "correct horse battery";

struct TestNode {
    name: String,
    handle: NodeHandle,
    clip: MemoryClipboard,
    dir: TempDir,
}

async fn start_in(name: &str, dir: &Path, clip: MemoryClipboard) -> NodeHandle {
    let cfg = Config {
        device_name: name.to_string(),
        port: 0,
        discovery: false,
        poll_interval_ms: 20,
        secret_store: SecretStoreKind::File,
        ..Config::default()
    };
    Node::start(dir, cfg, memory_clipboard(clip))
        .await
        .expect("node starts")
}

impl TestNode {
    async fn new(name: &str) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("aura=debug")
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let clip = MemoryClipboard::new();
        let handle = start_in(name, dir.path(), clip.clone()).await;
        Self {
            name: name.to_string(),
            handle,
            clip,
            dir,
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.handle.status().listen_port))
    }

    fn online_peers(&self) -> usize {
        self.handle.status().online_peers()
    }

    fn text(&self) -> Option<String> {
        match self.clip.current()? {
            ClipData::Text(t) => Some(t),
            other => Some(format!("{other:?}")),
        }
    }

    fn copy(&self, s: &str) {
        self.clip.user_copy(ClipData::Text(s.into()));
    }

    async fn restart(&mut self) {
        self.handle.shutdown().await;
        self.handle = start_in(&self.name, self.dir.path(), self.clip.clone()).await;
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A creates the group, B joins through A, C joins through B (and must find A via hints).
async fn trio() -> (TestNode, TestNode, TestNode) {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    let c = TestNode::new("charlie").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    c.handle
        .join(JoinTarget::Addr(b.addr()), PASS)
        .await
        .unwrap();
    eventually("full mesh", || {
        a.online_peers() == 2 && b.online_peers() == 2 && c.online_peers() == 2
    })
    .await;
    (a, b, c)
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_propagate_to_every_device_without_echo() {
    let (a, b, c) = trio().await;

    a.copy("hello from alpha");
    eventually("b and c receive", || {
        b.text().as_deref() == Some("hello from alpha")
            && c.text().as_deref() == Some("hello from alpha")
    })
    .await;

    c.copy("hi from charlie");
    eventually("a and b receive", || {
        a.text().as_deref() == Some("hi from charlie")
            && b.text().as_deref() == Some("hi from charlie")
    })
    .await;

    // Let any echo loop show itself.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        a.clip.engine_writes(),
        1,
        "alpha only applied charlie's entry"
    );
    assert_eq!(
        c.clip.engine_writes(),
        1,
        "charlie only applied alpha's entry"
    );
    for n in [&a, &b, &c] {
        let s = n.handle.status();
        assert_eq!(s.history.len(), 2, "{} history", n.name);
        assert_eq!(s.history[0].origin, "charlie");
        assert_eq!(s.group.as_ref().unwrap().name, "Home");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_copies_converge_to_one_winner() {
    let (a, b, c) = trio().await;
    a.copy("from a");
    b.copy("from b");
    c.copy("from c");
    eventually("all agree", || {
        let (x, y, z) = (a.text(), b.text(), c.text());
        x.is_some() && x == y && y == z && {
            let ids: Vec<_> = [&a, &b, &c]
                .iter()
                .map(|n| n.handle.status().history.first().map(|h| h.id.clone()))
                .collect();
            ids[0] == ids[1] && ids[1] == ids[2]
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passphrase_is_rejected() {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    let err = b
        .handle
        .join(JoinTarget::Addr(a.addr()), "not the passphrase")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("wrong passphrase"), "{err:#}");
    assert!(b.handle.status().group.is_none());
    assert_eq!(a.handle.status().members.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_device_is_dropped_and_can_rejoin_with_new_identity() {
    let (a, b, c) = trio().await;
    let c_id = c.handle.status().device_id.parse().unwrap();
    a.handle.remove_device(c_id).await.unwrap();
    eventually("charlie leaves", || c.handle.status().group.is_none()).await;
    eventually("others drop charlie", || {
        a.handle.status().members.len() == 2 && b.handle.status().members.len() == 2
    })
    .await;

    a.copy("secret after removal");
    eventually("b receives", || {
        b.text().as_deref() == Some("secret after removal")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_ne!(c.text().as_deref(), Some("secret after removal"));

    // Leaving regenerated charlie's identity, so it can join again.
    assert_ne!(c.handle.status().device_id, c_id.to_string());
    c.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("charlie back", || c.online_peers() == 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_device_catches_up_on_restart() {
    let a = TestNode::new("alpha").await;
    let mut b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    b.handle.shutdown().await;
    eventually("a notices", || a.online_peers() == 0).await;
    a.copy("copied while bravo slept");

    b.restart().await;
    eventually("b catches up", || {
        b.text().as_deref() == Some("copied while bravo slept")
    })
    .await;
    assert_eq!(b.handle.status().group.unwrap().name, "Home");
}

#[tokio::test(flavor = "multi_thread")]
async fn images_sync() {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    // ~1 MB of noisy RGBA so the PNG spans many Noise frames.
    let (w, h) = (512u32, 512u32);
    let mut seed = 1u32;
    let rgba: Vec<u8> = (0..w * h * 4)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let img = ClipData::Image {
        width: w,
        height: h,
        rgba,
    };
    a.clip.user_copy(img.clone());
    eventually("image arrives", || b.clip.current().as_ref() == Some(&img)).await;
    assert_eq!(b.handle.status().history[0].kind, "image");
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_device_neither_sends_nor_applies() {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    b.handle.set_paused(true).await.unwrap();
    b.copy("private on bravo");
    a.copy("from alpha");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(b.text().as_deref(), Some("private on bravo"));
    assert_eq!(a.text().as_deref(), Some("from alpha"));

    b.handle.set_paused(false).await.unwrap();
    eventually("b catches up after unpause", || {
        b.text().as_deref() == Some("from alpha")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recopy_makes_old_entry_current_everywhere() {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    a.copy("first");
    eventually("b first", || b.text().as_deref() == Some("first")).await;
    a.copy("second");
    eventually("b second", || b.text().as_deref() == Some("second")).await;

    let old = b
        .handle
        .status()
        .history
        .iter()
        .find(|h| h.preview == "first")
        .unwrap()
        .id
        .clone();
    b.handle
        .recopy(aura_node::EntryId::from_hex(&old).unwrap())
        .await
        .unwrap();
    eventually("both back to first", || {
        a.text().as_deref() == Some("first") && b.text().as_deref() == Some("first")
    })
    .await;

    // The recopied entry moves to the top instead of being listed twice, on both PCs.
    let previews = |n: &TestNode| -> Vec<String> {
        n.handle
            .status()
            .history
            .into_iter()
            .map(|h| h.preview)
            .collect()
    };
    eventually("histories deduplicated", || {
        previews(&a) == ["first", "second"] && previews(&b) == ["first", "second"]
    })
    .await;
    // Give a late echo or duplicate a chance to show up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(previews(&a), ["first", "second"]);
    assert_eq!(previews(&b), ["first", "second"]);
    // Alpha copied "first" and "second" itself; its only remote write is bravo's recopy.
    assert_eq!(a.clip.engine_writes(), 1);
    // Bravo applied alpha's two copies, then wrote the recopied entry locally.
    assert_eq!(b.clip.engine_writes(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn copying_the_same_text_again_does_not_duplicate_history() {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    a.copy("hello");
    eventually("b hello", || b.text().as_deref() == Some("hello")).await;
    b.copy("world");
    eventually("a world", || a.text().as_deref() == Some("world")).await;
    a.copy("hello");
    eventually("b hello again", || b.text().as_deref() == Some("hello")).await;

    for n in [&a, &b] {
        let previews: Vec<String> = n
            .handle
            .status()
            .history
            .into_iter()
            .map(|h| h.preview)
            .collect();
        assert_eq!(previews, ["hello", "world"], "{}", n.name);
    }
}

fn key_version(n: &TestNode) -> Option<(u64, String)> {
    n.handle
        .status()
        .group
        .map(|g| (g.passphrase_changed_ms, g.passphrase_changed_by))
}

#[tokio::test(flavor = "multi_thread")]
async fn passphrase_rotation_reaches_everyone_and_gates_new_joins() {
    let (a, b, c) = trio().await;
    let before = key_version(&a);
    a.handle
        .rotate_passphrase("brand new passphrase")
        .await
        .unwrap();
    eventually("all members have the new key", || {
        let v = key_version(&a);
        v != before && v == key_version(&b) && v == key_version(&c)
    })
    .await;
    assert_eq!(key_version(&c).unwrap().1, "alpha");

    // Join through charlie, who only learned the key through gossip.
    let d = TestNode::new("delta").await;
    let err = d
        .handle
        .join(JoinTarget::Addr(c.addr()), PASS)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("wrong passphrase"), "{err:#}");
    d.handle
        .join(JoinTarget::Addr(c.addr()), "brand new passphrase")
        .await
        .unwrap();
    eventually("delta meshes", || d.online_peers() == 3).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_member_gets_the_rotated_key_on_reconnect() {
    let a = TestNode::new("alpha").await;
    let mut b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    b.handle.shutdown().await;
    eventually("a notices", || a.online_peers() == 0).await;
    a.handle
        .rotate_passphrase("rotated while away")
        .await
        .unwrap();

    b.restart().await;
    eventually("b catches up on the key", || {
        key_version(&b) == key_version(&a)
    })
    .await;
    let c = TestNode::new("charlie").await;
    c.handle
        .join(JoinTarget::Addr(b.addr()), "rotated while away")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_rotations_converge() {
    let (a, b, c) = trio().await;
    let (ra, rb) = tokio::join!(
        a.handle.rotate_passphrase("alpha's new passphrase"),
        b.handle.rotate_passphrase("bravo's new passphrase"),
    );
    // One of them may lose the race locally; at least one must succeed.
    assert!(ra.is_ok() || rb.is_ok());
    eventually("everyone agrees on one key", || {
        let v = key_version(&a);
        v == key_version(&b) && v == key_version(&c)
    })
    .await;
    let winner = key_version(&a).unwrap().1;
    let pass = if winner == "alpha" {
        "alpha's new passphrase"
    } else {
        "bravo's new passphrase"
    };
    let d = TestNode::new("delta").await;
    d.handle
        .join(JoinTarget::Addr(c.addr()), pass)
        .await
        .unwrap();
}

/// (fofoqueiro name as each node sees it, whether the node serves).
fn web_view(n: &TestNode) -> (Option<String>, bool) {
    let s = n.handle.status();
    (s.fofoqueiro, s.is_fofoqueiro)
}

/// The node with the lowest device id among `nodes`.
fn lowest<'a>(nodes: &[&'a TestNode]) -> &'a TestNode {
    nodes
        .iter()
        .min_by_key(|n| n.handle.status().device_id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn phone_page_is_served_by_one_elected_pc_with_failover() {
    let (a, b, c) = trio().await;
    let winner = lowest(&[&a, &b, &c]).name.clone();
    eventually("everyone agrees and exactly one serves", || {
        let views = [web_view(&a), web_view(&b), web_view(&c)];
        views
            .iter()
            .all(|(f, _)| f.as_deref() == Some(winner.as_str()))
            && views.iter().filter(|(_, serving)| *serving).count() == 1
    })
    .await;

    // Stop the fofoqueiro: the next-lowest PC takes over.
    let (mut gone, rest): (Vec<TestNode>, Vec<TestNode>) =
        [a, b, c].into_iter().partition(|n| n.name == winner);
    gone.pop().unwrap().handle.shutdown().await;
    let next = lowest(&[&rest[0], &rest[1]]).name.clone();
    eventually("the next PC takes over", || {
        let views = [web_view(&rest[0]), web_view(&rest[1])];
        views
            .iter()
            .all(|(f, _)| f.as_deref() == Some(next.as_str()))
            && views.iter().filter(|(_, serving)| *serving).count() == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opted_out_pc_is_never_elected() {
    let (a, b, c) = trio().await;
    let nodes = [&a, &b, &c];
    let first = lowest(&nodes);
    first.handle.set_web_enabled(false).await.unwrap();
    let others: Vec<&TestNode> = nodes.into_iter().filter(|n| n.name != first.name).collect();
    let next = lowest(&others).name.clone();
    eventually("the opted-out PC is skipped", || {
        [&a, &b, &c]
            .iter()
            .all(|n| web_view(n).0.as_deref() == Some(next.as_str()))
            && !web_view(first).1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_member_shares_the_phone_page_certificate() {
    let (a, b, c) = trio().await;
    let cert = a
        .handle
        .web_cert()
        .await
        .unwrap()
        .expect("created with the group");
    assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
    for n in [&b, &c] {
        assert_eq!(
            n.handle.web_cert().await.unwrap(),
            Some(cert.clone()),
            "{}",
            n.name
        );
    }
    // Session keys match too, so a phone login works on whichever PC serves.
    let k = a.handle.web_session_key().await.unwrap();
    assert_eq!(b.handle.web_session_key().await.unwrap(), k);
    assert!(a.handle.verify_passphrase(PASS).await.unwrap());
    assert!(
        !a.handle
            .verify_passphrase("wrong passphrase")
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fuzzy_search_works_live_over_the_control_channel_and_offline() {
    let a = TestNode::new("alpha").await;
    let mut b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", || a.online_peers() == 1).await;

    for text in [
        "cargo test -p aura-web",
        "git status",
        "the wifi password is hunter2",
    ] {
        a.copy(text);
        eventually("b got it", || b.text().as_deref() == Some(text)).await;
    }

    // Letters in order, and a typo.
    let hits = b.handle.search("cgtst", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text.as_deref(), Some("cargo test -p aura-web"));
    assert_eq!(hits[0].origin, "alpha");
    let lit: String = hits[0]
        .snippet
        .iter()
        .filter(|p| p.hit)
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(lit, "cgtst");
    let hits = b.handle.search("pasword", 10).await.unwrap();
    assert_eq!(
        hits[0].text.as_deref(),
        Some("the wifi password is hunter2")
    );

    // The CLI's control channel gives the same results.
    let client = aura_node::control::Client::connect(b.dir.path())
        .await
        .expect("b is running");
    assert_eq!(
        client.search("cgtst", 10).await.unwrap(),
        b.handle.search("cgtst", 10).await.unwrap()
    );
    let err = aura_node::control::Client::connect(b.dir.path())
        .await
        .unwrap()
        .with_token("not the token")
        .search("git", 10)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bad token"), "{err:#}");

    // Recopy through the channel reaches every PC.
    let git = client.search("git status", 1).await.unwrap().remove(0);
    client.recopy(&git.id).await.unwrap();
    eventually("alpha got the recopy", || {
        a.text().as_deref() == Some("git status")
    })
    .await;

    // With bravo stopped, the CLI searches its database directly.
    b.handle.shutdown().await;
    eventually("control file removed", || {
        !b.dir.path().join("control.json").exists()
    })
    .await;
    assert!(
        aura_node::control::Client::connect(b.dir.path())
            .await
            .is_none()
    );
    let offline = aura_node::search_offline(b.dir.path(), "cgtst", 10).unwrap();
    assert_eq!(offline[0].text.as_deref(), Some("cargo test -p aura-web"));
    assert_eq!(offline[0].origin, "alpha");
    b.restart().await;
}
