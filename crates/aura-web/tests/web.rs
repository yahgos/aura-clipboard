//! The phone page end to end: real nodes with in-memory clipboards, a real HTTPS
//! server, and a client that trusts the group's certificate like a phone would after
//! accepting it once.

use std::path::Path;
use std::time::Duration;

use aura_clip::ClipData;
use aura_node::{
    Config, JoinTarget, MemoryClipboard, Node, NodeHandle, SecretStoreKind, memory_clipboard,
};
use aura_web::{WebServer, session};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const PASS: &str = "correct horse battery";

struct TestNode {
    handle: NodeHandle,
    clip: MemoryClipboard,
    _dir: TempDir,
}

async fn start_in(name: &str, dir: &Path, clip: MemoryClipboard, web_port: u16) -> NodeHandle {
    let cfg = Config {
        device_name: name.to_string(),
        port: 0,
        discovery: false,
        poll_interval_ms: 20,
        secret_store: SecretStoreKind::File,
        web_port,
        ..Config::default()
    };
    Node::start(dir, cfg, memory_clipboard(clip)).await.unwrap()
}

impl TestNode {
    async fn new(name: &str) -> Self {
        Self::with_web_port(name, 0).await
    }

    async fn with_web_port(name: &str, web_port: u16) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("aura=debug")
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let clip = MemoryClipboard::new();
        let handle = start_in(name, dir.path(), clip.clone(), web_port).await;
        Self {
            handle,
            clip,
            _dir: dir,
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.handle.status().listen_port))
    }

    fn text(&self) -> Option<String> {
        match self.clip.current()? {
            ClipData::Text(t) => Some(t),
            _ => None,
        }
    }
}

async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Two PCs in one group.
async fn pair() -> (TestNode, TestNode) {
    let a = TestNode::new("alpha").await;
    let b = TestNode::new("bravo").await;
    a.handle.create_group("Home", PASS).await.unwrap();
    b.handle
        .join(JoinTarget::Addr(a.addr()), PASS)
        .await
        .unwrap();
    eventually("connected", async || a.handle.status().online_peers() == 1).await;
    (a, b)
}

/// A phone: keeps cookies and trusts the group certificate.
async fn phone(node: &NodeHandle) -> reqwest::Client {
    let cert = node.web_cert().await.unwrap().unwrap();
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert.cert_pem.as_bytes()).unwrap())
        .cookie_store(true)
        .build()
        .unwrap()
}

fn url(server: &WebServer, path: &str) -> String {
    format!("https://localhost:{}{path}", server.local_addr().port())
}

async fn login(client: &reqwest::Client, server: &WebServer, passphrase: &str) -> StatusCode {
    client
        .post(url(server, "/api/login"))
        .json(&json!({ "passphrase": passphrase }))
        .send()
        .await
        .unwrap()
        .status()
}

async fn history(client: &reqwest::Client, server: &WebServer) -> (StatusCode, Value) {
    let res = client
        .get(url(server, "/api/history"))
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn phone_logs_in_reads_history_and_images() {
    let (a, b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;

    // The page itself loads without a session; data does not.
    let page = client.get(url(&server, "/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    assert_eq!(history(&client, &server).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        login(&client, &server, "wrong passphrase").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(login(&client, &server, PASS).await, StatusCode::OK);

    a.clip.user_copy(ClipData::Text("copied on alpha".into()));
    // Let the text land first so the image is clearly the newer entry.
    eventually("text reaches bravo", async || {
        b.text().as_deref() == Some("copied on alpha")
    })
    .await;
    let img = ClipData::Image {
        width: 2,
        height: 1,
        rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
    };
    b.clip.user_copy(img.clone());
    eventually("both entries on the page", async || {
        history(&client, &server).await.1["items"]
            .as_array()
            .is_some_and(|items| items.len() == 2)
    })
    .await;

    let (_, body) = history(&client, &server).await;
    assert_eq!(body["group"], "Home");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["kind"], "image");
    assert_eq!(items[0]["origin"], "bravo");
    assert_eq!(items[1]["text"], "copied on alpha");

    let png = client
        .get(url(
            &server,
            &format!("/api/entries/{}/image", items[0]["id"].as_str().unwrap()),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(png.headers()["content-type"], "image/png");
    let decoded = ClipData::from_content(&aura_node::Content::ImagePng {
        width: 2,
        height: 1,
        png: png.bytes().await.unwrap().to_vec(),
    })
    .unwrap();
    assert_eq!(decoded, img);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passphrases_are_rate_limited() {
    let (a, _b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;
    for _ in 0..5 {
        assert_eq!(
            login(&client, &server, "nope nope nope").await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&client, &server, PASS).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn text_sent_from_the_phone_reaches_every_pc_once() {
    let (a, b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;
    assert_eq!(login(&client, &server, PASS).await, StatusCode::OK);

    let res = client
        .post(url(&server, "/api/send"))
        .json(&json!({ "text": "typed on the phone" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    eventually("both PCs have it", async || {
        a.text().as_deref() == Some("typed on the phone")
            && b.text().as_deref() == Some("typed on the phone")
    })
    .await;
    let (_, body) = history(&client, &server).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["origin"], "alpha");

    // A cross-site form post can't send JSON, so it is rejected before anything happens.
    let form = client
        .post(url(&server, "/api/send"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("text=forged")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test(flavor = "multi_thread")]
async fn login_survives_failover_and_ends_with_a_passphrase_change() {
    let (a, b) = pair().await;
    let server_a = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;
    assert_eq!(login(&client, &server_a, PASS).await, StatusCode::OK);

    // Bravo takes over serving: the same cookie (and the same certificate) work there.
    drop(server_a);
    let server_b = WebServer::start(b.handle.clone(), 0).await.unwrap();
    assert_eq!(history(&client, &server_b).await.0, StatusCode::OK);

    b.handle
        .rotate_passphrase("a brand new passphrase")
        .await
        .unwrap();
    assert_eq!(
        history(&client, &server_b).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&client, &server_b, "a brand new passphrase").await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn qr_code_logs_a_phone_in_until_it_expires() {
    let (a, _b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let link = aura_web::qr_login_url(&a.handle, "localhost", server.local_addr().port())
        .await
        .unwrap();
    let token = link.split_once("#t=").unwrap().1.to_string();
    assert!(aura_web::qr_svg(&link).unwrap().starts_with("<?xml"));

    let client = phone(&a.handle).await;
    let res = client
        .post(url(&server, "/api/login"))
        .json(&json!({ "qr_token": token }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(history(&client, &server).await.0, StatusCode::OK);

    let key = a.handle.web_session_key().await.unwrap();
    let expired = session::issue(&key, session::Kind::QrLogin, session::now_secs() - 1);
    let other = phone(&a.handle).await;
    let res = other
        .post(url(&server, "/api/login"))
        .json(&json!({ "qr_token": expired }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_updates_arrive_over_server_sent_events() {
    let (a, _b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;
    assert_eq!(login(&client, &server, PASS).await, StatusCode::OK);

    let mut res = client
        .get(url(&server, "/api/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut seen = String::new();
    // Reads the stream until one more `changed` event than before has arrived.
    async fn next_changed(res: &mut reqwest::Response, seen: &mut String) {
        let before = seen.matches("event: changed").count();
        while seen.matches("event: changed").count() == before {
            let chunk = res.chunk().await.unwrap().expect("stream ended");
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    tokio::time::timeout(Duration::from_secs(5), next_changed(&mut res, &mut seen))
        .await
        .expect("initial event");
    a.clip.user_copy(ClipData::Text("live!".into()));
    tokio::time::timeout(Duration::from_secs(5), next_changed(&mut res, &mut seen))
        .await
        .expect("event after a copy");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_elected_pc_starts_serving_on_its_own() {
    // Reserve a free port for the page.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let a = TestNode::with_web_port("alpha", port).await;
    a.handle.create_group("Home", PASS).await.unwrap();
    let _supervisor = aura_web::spawn(a.handle.clone());
    let client = phone(&a.handle).await;
    eventually("page is up", async || {
        client
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .is_ok_and(|r| r.status() == StatusCode::OK)
    })
    .await;
    assert!(a.handle.status().is_fofoqueiro);
}

#[tokio::test(flavor = "multi_thread")]
async fn page_answers_over_ipv6_and_ipv4() {
    // Phones often get aura.local's IPv6 addresses first, so the page must take both.
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipping: no IPv6 on this machine");
        return;
    }
    let (a, _b) = pair().await;
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let port = server.local_addr().port();
    let cert = a.handle.web_cert().await.unwrap().unwrap();
    for ip in ["::1", "127.0.0.1"] {
        // Pin "localhost" to one address family; TLS still checks the name.
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(cert.cert_pem.as_bytes()).unwrap())
            .resolve(
                "localhost",
                std::net::SocketAddr::new(ip.parse().unwrap(), port),
            )
            .build()
            .unwrap();
        let res = client
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("no answer over {ip}: {e}"));
        assert_eq!(res.status(), StatusCode::OK, "over {ip}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn phone_can_search_and_recent_list_is_capped() {
    let (a, _b) = pair().await;
    for i in 0..60 {
        a.handle
            .publish(aura_node::Content::Text(format!("note number {i}")))
            .await
            .unwrap();
    }
    a.handle
        .publish(aura_node::Content::Text("cargo test -p aura-web".into()))
        .await
        .unwrap();
    let server = WebServer::start(a.handle.clone(), 0).await.unwrap();
    let client = phone(&a.handle).await;
    assert_eq!(login(&client, &server, PASS).await, StatusCode::OK);

    let (_, recent) = history(&client, &server).await;
    assert_eq!(
        recent["items"].as_array().unwrap().len(),
        50,
        "only the newest 50 without a search"
    );

    let found: Value = client
        .get(url(&server, "/api/history"))
        .query(&[("q", "cgtst")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["query"], "cgtst");
    let items = found["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["text"], "cargo test -p aura-web");
    let lit: String = items[0]["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["hit"] == true)
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    assert_eq!(lit, "cgtst");

    // Older entries beyond the recent list are still searchable.
    let found: Value = client
        .get(url(&server, "/api/history"))
        .query(&[("q", "number 3")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        found["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["text"] == "note number 3")
    );
}
