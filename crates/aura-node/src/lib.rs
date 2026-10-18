//! Aura Clipboard engine. [`Node::start`] wires the OS clipboard, the network
//! and storage together and returns a [`NodeHandle`] for UIs (tray, CLI).

mod actor;
mod api;
pub mod config;
pub mod control;
mod secrets;
mod store;
pub mod webcert;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use anyhow::{Context, Result};
pub use aura_clip::{ClipboardBackend, MemoryClipboard, SystemClipboard};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

pub use api::{
    DiscoveredGroup, Event, GroupStatus, HistoryItem, JoinTarget, MemberStatus, NodeHandle,
    SearchResult, Status,
};
pub use aura_core::{Content, DeviceId, Entry, EntryId, GroupId, SnippetPart};
pub use aura_net::proto::WebCert;
pub use config::{Config, SecretStoreKind, default_data_dir};

use actor::{Actor, ActorMsg, ActorParts, Internal};
use secrets::SecretStore;
use store::Store;

/// Creates the clipboard backend on the clipboard thread.
pub type ClipboardFactory = Box<dyn FnOnce() -> Result<Box<dyn ClipboardBackend>> + Send>;

pub fn system_clipboard() -> ClipboardFactory {
    Box::new(|| Ok(Box::new(SystemClipboard::new()?) as Box<dyn ClipboardBackend>))
}

pub fn memory_clipboard(clip: MemoryClipboard) -> ClipboardFactory {
    Box::new(move || Ok(Box::new(clip) as Box<dyn ClipboardBackend>))
}

pub struct Node;

impl Node {
    /// Starts the node in the current tokio runtime.
    pub async fn start(
        data_dir: &Path,
        cfg: Config,
        clipboard: ClipboardFactory,
    ) -> Result<NodeHandle> {
        let store = Store::open(data_dir)?;
        let secret_store = SecretStore::new(cfg.secret_store, data_dir);
        let secrets = secret_store
            .load_or_create()
            .context("loading device secrets")?;
        let listener = bind(cfg.port).await?;
        tracing::info!(
            "device {} ({}) listening on {}",
            cfg.device_name,
            secrets.identity.device_id().short(),
            listener.local_addr()?
        );

        let (tx, rx) = mpsc::channel::<ActorMsg>(256);
        let (status_tx, status_rx) = watch::channel(api::Status::default());
        let (events_tx, _) = broadcast::channel(64);

        let copy_tx = tx.clone();
        let clip = aura_clip::ClipboardWorker::spawn(
            clipboard,
            cfg.poll_interval(),
            move |content, writes_seen| {
                let _ = copy_tx.blocking_send(ActorMsg::Int(Internal::LocalCopy {
                    content,
                    writes_seen,
                }));
            },
        )
        .context("opening the clipboard")?;

        let actor = Actor::new(ActorParts {
            cfg,
            secrets,
            secret_store,
            store,
            clip,
            tx: tx.clone(),
            status_tx,
            events_tx: events_tx.clone(),
            listener,
            data_dir: data_dir.to_path_buf(),
        })?;
        tokio::spawn(async move {
            // `run` consumes the actor, so the database lock is released before we ack.
            if let Some(ack) = actor.run(rx).await {
                let _ = ack.send(());
            }
        });
        let mut handle = NodeHandle {
            cmd_tx: tx,
            status_rx,
            events_tx,
        };
        // Wait for the first real status.
        let _ = handle.status_rx.changed().await;
        // Lets the `aura` CLI reach this node (e.g. `aura search`).
        let control_node = handle.clone();
        let control_dir = data_dir.to_path_buf();
        tokio::spawn(async move {
            if let Err(e) = control::serve(control_node, control_dir).await {
                tracing::warn!("control channel unavailable: {e:#}");
            }
        });
        Ok(handle)
    }
}

/// This PC's primary LAN address: the source address the OS would use to reach the
/// internet. Connecting a UDP socket sends no packets.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// Resolves what a user typed to reach another PC: `host`, `host:port`, `ip` or
/// `ip:port` (the default port is assumed when missing).
pub async fn resolve_peer(addr: &str) -> Result<SocketAddr> {
    let addr = addr.trim();
    let with_port = if addr
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        addr.to_string()
    } else {
        format!("{addr}:{}", config::DEFAULT_PORT)
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("resolving {addr}"))?
        .find(|a| a.is_ipv4())
        .with_context(|| format!("no IPv4 address for {addr}"))
}

/// Searches the history of a node that is *not* running, straight from its database
/// (the database is locked while Aura runs; use [`control::Client`] then).
pub fn search_offline(data_dir: &Path, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let store = Store::open(data_dir)?;
    let mut entries = store.load_history()?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.id));
    let membership = store.membership()?;
    let names = entries
        .iter()
        .filter_map(|e| {
            let m = membership.get_any(&e.id.origin)?;
            Some((e.id.origin, m.name.clone()))
        })
        .collect();
    Ok(api::search_results(&entries, &names, query, limit))
}

/// One history entry of a node that is not running.
pub fn entry_offline(data_dir: &Path, id: EntryId) -> Result<Option<Entry>> {
    let store = Store::open(data_dir)?;
    Ok(store.load_history()?.into_iter().find(|e| e.id == id))
}

async fn bind(port: u16) -> Result<TcpListener> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on port {port}"))
}
