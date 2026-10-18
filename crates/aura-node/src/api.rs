use std::net::SocketAddr;

use anyhow::{Result, anyhow};
use aura_core::{Content, DeviceId, Entry, EntryId, GroupId};
use aura_net::proto::WebCert;
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
    /// This PC takes part in the phone-page election.
    pub web_enabled: bool,
    pub web_port: u16,
    /// Name of the PC that serves (or is about to serve) the phone page.
    pub fofoqueiro: Option<String>,
    /// LAN address of that PC, for phones that can't resolve `aura.local`.
    pub fofoqueiro_ip: Option<String>,
    /// This PC is serving the phone page right now.
    pub is_fofoqueiro: bool,
    /// Expiry of the group's phone-page certificate; changes when it is renewed.
    pub web_cert_expires_ms: Option<u64>,
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
    LocalCopy {
        preview: String,
    },
    RemoteApplied {
        from: String,
        preview: String,
    },
    PeerConnected {
        name: String,
    },
    PeerDisconnected {
        name: String,
    },
    MemberJoined {
        name: String,
    },
    MemberRemoved {
        name: String,
    },
    GroupJoined {
        name: String,
    },
    GroupLeft,
    /// This PC started or stopped serving the phone page.
    PhonePage {
        serving: bool,
    },
    PassphraseChanged {
        by: String,
    },
    JoinRejected {
        from_addr: String,
    },
    Warning {
        message: String,
    },
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
    Entries {
        reply: Reply<Vec<Entry>>,
    },
    Entry {
        id: EntryId,
        reply: Reply<Option<Entry>>,
    },
    WebSecrets {
        reply: Reply<WebSecrets>,
    },
    Publish {
        content: Content,
        reply: Reply,
    },
    SetWebEnabled(bool),
    Shutdown(oneshot::Sender<()>),
}

pub(crate) struct WebSecrets {
    pub group_id: GroupId,
    pub password_key: [u8; 32],
    pub web_cert: Option<WebCert>,
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

    /// Full history, newest first.
    pub async fn entries(&self) -> Result<Vec<Entry>> {
        self.call(|reply| Command::Entries { reply }).await
    }

    pub async fn entry(&self, id: EntryId) -> Result<Option<Entry>> {
        self.call(|reply| Command::Entry { id, reply }).await
    }

    /// Checks a passphrase against the group's current key (Argon2, so it takes a moment).
    pub async fn verify_passphrase(&self, passphrase: &str) -> Result<bool> {
        let secrets = self.call(|reply| Command::WebSecrets { reply }).await?;
        let passphrase = passphrase.to_string();
        let candidate = tokio::task::spawn_blocking(move || {
            aura_net::enroll::password_key(&passphrase, secrets.group_id)
        })
        .await??;
        Ok(constant_time_eq(&candidate, &secrets.password_key))
    }

    /// Key for signing phone-page sessions. Derived from the group key, so a session is
    /// valid on every member and a passphrase change logs every phone out.
    pub async fn web_session_key(&self) -> Result<[u8; 32]> {
        let secrets = self.call(|reply| Command::WebSecrets { reply }).await?;
        Ok(blake3::derive_key(
            "aura-clipboard v2 web session",
            &secrets.password_key,
        ))
    }

    /// The group's shared phone-page certificate, if one exists yet.
    pub async fn web_cert(&self) -> Result<Option<WebCert>> {
        Ok(self
            .call(|reply| Command::WebSecrets { reply })
            .await?
            .web_cert)
    }

    /// Makes `content` the current clipboard on this PC and every other, as if it had
    /// been copied here. Used by the phone page's "Send to PCs".
    pub async fn publish(&self, content: Content) -> Result<()> {
        self.call(|reply| Command::Publish { content, reply }).await
    }

    /// Opts this PC in or out of serving the phone page (saved to config.toml).
    pub async fn set_web_enabled(&self, enabled: bool) -> Result<()> {
        self.send(Command::SetWebEnabled(enabled)).await
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
