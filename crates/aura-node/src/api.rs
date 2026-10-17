use std::net::SocketAddr;

use anyhow::{Result, anyhow};
use aura_core::{DeviceId, EntryId, GroupId};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// Snapshot of the node for UIs. Published on a watch channel; only changes when
/// something visible changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub device_id: String,
    pub device_name: String,
    pub listen_port: u16,
    /// This PC's address on the LAN, for joining by address when discovery fails.
    pub lan_ip: Option<String>,
    pub group: Option<GroupStatus>,
    pub members: Vec<MemberStatus>,
    pub paused: bool,
    /// Newest first.
    pub history: Vec<HistoryItem>,
    /// Groups seen on the network via mDNS (other than ours).
    pub discovered_groups: Vec<DiscoveredGroup>,
}

impl Status {
    pub fn online_peers(&self) -> usize {
        self.members
            .iter()
            .filter(|m| m.online && !m.is_self)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupStatus {
    pub id: String,
    pub name: String,
    /// When the current passphrase was set (group creation or last change).
    pub passphrase_changed_ms: u64,
    pub passphrase_changed_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberStatus {
    pub device_id: String,
    pub name: String,
    pub online: bool,
    pub is_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryItem {
    pub id: String,
    pub kind: String,
    pub preview: String,
    pub size: usize,
    pub origin: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredGroup {
    pub group_id: String,
    pub name: String,
    pub devices: Vec<String>,
}

/// Things worth telling the user about (notifications, logs).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    LocalCopy { preview: String },
    RemoteApplied { from: String, preview: String },
    PeerConnected { name: String },
    PeerDisconnected { name: String },
    MemberJoined { name: String },
    MemberRemoved { name: String },
    GroupJoined { name: String },
    GroupLeft,
    PassphraseChanged { by: String },
    JoinRejected { from_addr: String },
    Warning { message: String },
}

pub enum JoinTarget {
    /// A device address (manual `host:port`).
    Addr(SocketAddr),
    /// A group seen through mDNS; any of its devices will do.
    Group(GroupId),
}

pub(crate) type Reply<T = ()> = oneshot::Sender<Result<T>>;

pub(crate) enum Command {
    CreateGroup {
        name: String,
        passphrase: String,
        reply: Reply,
    },
    Join {
        target: JoinTarget,
        passphrase: String,
        reply: Reply,
    },
    RotatePassphrase {
        passphrase: String,
        reply: Reply,
    },
    Leave {
        reply: Reply,
    },
    RemoveDevice {
        id: DeviceId,
        reply: Reply,
    },
    SetPaused(bool),
    Recopy {
        id: EntryId,
        reply: Reply,
    },
    AddPeer(SocketAddr),
    Shutdown(oneshot::Sender<()>),
}

fn check_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < 8 {
        return Err(anyhow!("passphrase must have at least 8 characters"));
    }
    Ok(())
}

/// Cheap, cloneable handle to a running node.
#[derive(Clone)]
pub struct NodeHandle {
    pub(crate) cmd_tx: mpsc::Sender<crate::actor::ActorMsg>,
    pub(crate) status_rx: watch::Receiver<Status>,
    pub(crate) events_tx: broadcast::Sender<Event>,
}

impl NodeHandle {
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx)).await?;
        rx.await.map_err(|_| anyhow!("node stopped"))?
    }

    async fn send(&self, cmd: Command) -> Result<()> {
        self.cmd_tx
            .send(crate::actor::ActorMsg::Cmd(cmd))
            .await
            .map_err(|_| anyhow!("node stopped"))
    }

    pub fn status(&self) -> Status {
        self.status_rx.borrow().clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<Status> {
        self.status_rx.clone()
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    pub async fn create_group(&self, name: &str, passphrase: &str) -> Result<()> {
        let (name, passphrase) = (name.trim().to_string(), passphrase.to_string());
        if name.is_empty() {
            return Err(anyhow!("group name is empty"));
        }
        check_passphrase(&passphrase)?;
        self.call(|reply| Command::CreateGroup {
            name,
            passphrase,
            reply,
        })
        .await
    }

    pub async fn join(&self, target: JoinTarget, passphrase: &str) -> Result<()> {
        let passphrase = passphrase.to_string();
        self.call(|reply| Command::Join {
            target,
            passphrase,
            reply,
        })
        .await
    }

    /// Changes the group passphrase on every member. Devices that already joined stay
    /// in; new devices need the new passphrase. Do this after removing a lost PC.
    pub async fn rotate_passphrase(&self, passphrase: &str) -> Result<()> {
        let passphrase = passphrase.to_string();
        check_passphrase(&passphrase)?;
        self.call(|reply| Command::RotatePassphrase { passphrase, reply })
            .await
    }

    pub async fn leave(&self) -> Result<()> {
        self.call(|reply| Command::Leave { reply }).await
    }

    pub async fn remove_device(&self, id: DeviceId) -> Result<()> {
        self.call(|reply| Command::RemoveDevice { id, reply }).await
    }

    pub async fn set_paused(&self, paused: bool) -> Result<()> {
        self.send(Command::SetPaused(paused)).await
    }

    /// Makes an old history entry the current clipboard everywhere.
    pub async fn recopy(&self, id: EntryId) -> Result<()> {
        self.call(|reply| Command::Recopy { id, reply }).await
    }

    pub async fn add_peer(&self, addr: SocketAddr) -> Result<()> {
        self.send(Command::AddPeer(addr)).await
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.send(Command::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}
