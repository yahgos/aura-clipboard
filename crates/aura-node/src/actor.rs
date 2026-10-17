//! The node actor: a single task that owns all state. Connection tasks, the
//! listener, the clipboard thread and mDNS talk to it through one channel.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use aura_clip::ClipboardWorker;
use aura_core::{
    Content, DeviceId, Entry, EntryId, GroupId, History, HlcClock, InsertOutcome, Member,
    Membership, PROTOCOL_VERSION,
};
use aura_net::discovery::{Advert, Discovered, Discovery, DiscoveryEvent};
use aura_net::enroll::{self, JoinAuthority, JoinError, RateLimiter};
use aura_net::proto::{GroupKey, Hello, JoinInfo, KeyVersion, Msg, Preamble, Welcome};
use aura_net::{Identity, Incoming, Session, session};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::api::{
    Command, DiscoveredGroup, Event, GroupStatus, HistoryItem, JoinTarget, MemberStatus, Reply,
    Status,
};
use crate::config::Config;
use crate::secrets::{SecretStore, Secrets};
use crate::store::{GroupInfo, Store};

const TICK: Duration = Duration::from_secs(2);
const KEEPALIVE: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const PEER_QUEUE: usize = 64;
const STATUS_HISTORY: usize = 20;

pub(crate) enum ActorMsg {
    Cmd(Command),
    Int(Internal),
}

pub(crate) enum Internal {
    /// `writes_seen`: clipboard writes the worker had applied before reading this copy.
    LocalCopy {
        content: Content,
        writes_seen: u64,
    },
    Session {
        session: Session,
        outbound: bool,
        dialed: Option<SocketAddr>,
    },
    Inbound {
        peer: DeviceId,
        conn: u64,
        msg: Msg,
    },
    Closed {
        peer: DeviceId,
        conn: u64,
    },
    DialFailed {
        addr: SocketAddr,
    },
    JoinAuthority(oneshot::Sender<Option<JoinAuthority>>),
    Admit {
        public_key: [u8; 32],
        info: JoinInfo,
        ip: IpAddr,
        reply: oneshot::Sender<Result<Welcome>>,
    },
    JoinFinished {
        result: Result<(Welcome, [u8; 32], SocketAddr), JoinError>,
        reply: Reply,
    },
    JoinRejected(SocketAddr),
    Discovery(DiscoveryEvent),
    Tick,
}

struct Peer {
    conn: u64,
    tx: mpsc::Sender<Arc<Vec<u8>>>,
    /// Who opened this TCP connection; used to break ties between duplicate connections.
    dialer: DeviceId,
    addr: SocketAddr,
}

struct Group {
    info: GroupInfo,
    key: GroupKey,
    membership: Membership,
}

pub(crate) struct Actor {
    pub cfg: Config,
    pub identity: Arc<RwLock<Identity>>,
    pub secrets: Secrets,
    pub secret_store: SecretStore,
    pub store: Store,
    group: Option<Group>,
    history: History,
    clock: HlcClock,
    peers: HashMap<DeviceId, Peer>,
    addr_book: BTreeMap<DeviceId, BTreeSet<SocketAddr>>,
    static_peers: BTreeSet<SocketAddr>,
    dialing: HashSet<SocketAddr>,
    backoff: HashMap<SocketAddr, (Instant, Duration)>,
    discovered: HashMap<DeviceId, Discovered>,
    discovery: Option<Discovery>,
    paused: bool,
    clip: ClipboardWorker,
    clip_writes: u64,
    tx: mpsc::Sender<ActorMsg>,
    status_tx: watch::Sender<Status>,
    events_tx: broadcast::Sender<Event>,
    next_conn: u64,
    listen_port: u16,
    lan_ip: Option<IpAddr>,
    tasks: Vec<JoinHandle<()>>,
}

pub(crate) struct ActorParts {
    pub cfg: Config,
    pub secrets: Secrets,
    pub secret_store: SecretStore,
    pub store: Store,
    pub clip: ClipboardWorker,
    pub tx: mpsc::Sender<ActorMsg>,
    pub status_tx: watch::Sender<Status>,
    pub events_tx: broadcast::Sender<Event>,
    pub listener: TcpListener,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn encode(msg: &Msg) -> Arc<Vec<u8>> {
    Arc::new(postcard::to_stdvec(msg).expect("messages always serialize"))
}

impl Actor {
    pub fn new(p: ActorParts) -> Result<Self> {
        let listen_port = p.listener.local_addr()?.port();
        let group = match (p.store.group()?, p.secrets.password_key) {
            (Some(info), Some(password_key)) => Some(Group {
                info,
                key: GroupKey {
                    version: p.store.key_version()?,
                    password_key,
                },
                membership: p.store.membership()?,
            }),
            (Some(info), None) => {
                tracing::warn!("group {} has no stored key; leaving it", info.name);
                None
            }
            _ => None,
        };
        let mut history = History::new(p.cfg.history_entries, p.cfg.history_bytes);
        let mut clock = HlcClock::new();
        for e in p.store.load_history()? {
            clock.observe(e.id.hlc, now_ms());
            history.insert(e);
        }
        let addr_book = p.store.addr_book()?.into_iter().collect();
        let identity = Arc::new(RwLock::new(p.secrets.identity.clone()));
        let mut actor = Self {
            static_peers: p.cfg.static_peers.iter().copied().collect(),
            paused: p.store.paused()?,
            cfg: p.cfg,
            identity: identity.clone(),
            secrets: p.secrets,
            secret_store: p.secret_store,
            store: p.store,
            group,
            history,
            clock,
            peers: HashMap::new(),
            addr_book,
            dialing: HashSet::new(),
            backoff: HashMap::new(),
            discovered: HashMap::new(),
            discovery: None,
            clip: p.clip,
            clip_writes: 0,
            tx: p.tx.clone(),
            status_tx: p.status_tx,
            events_tx: p.events_tx,
            next_conn: 0,
            listen_port,
            lan_ip: crate::lan_ip(),
            tasks: Vec::new(),
        };

        actor
            .tasks
            .push(tokio::spawn(listen(p.listener, identity, p.tx.clone())));
        let tick_tx = p.tx.clone();
        actor.tasks.push(tokio::spawn(async move {
            let mut iv = tokio::time::interval(TICK);
            loop {
                iv.tick().await;
                if tick_tx.send(ActorMsg::Int(Internal::Tick)).await.is_err() {
                    break;
                }
            }
        }));
        if actor.cfg.discovery {
            match Discovery::start(Some(&actor.advert())) {
                Ok((d, mut rx)) => {
                    actor.discovery = Some(d);
                    let tx = p.tx;
                    actor.tasks.push(tokio::spawn(async move {
                        while let Some(ev) = rx.recv().await {
                            if tx
                                .send(ActorMsg::Int(Internal::Discovery(ev)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }));
                }
                Err(e) => tracing::warn!("mDNS discovery unavailable: {e:#}"),
            }
        }
        Ok(actor)
    }

    /// Runs until shutdown. Returns the shutdown acknowledgement, to be sent only after
    /// the actor (and the database lock it holds) has been dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<ActorMsg>) -> Option<oneshot::Sender<()>> {
        self.publish_status();
        let mut done = None;
        while let Some(msg) = rx.recv().await {
            match msg {
                ActorMsg::Cmd(Command::Shutdown(ack)) => {
                    done = Some(ack);
                    break;
                }
                ActorMsg::Cmd(cmd) => self.on_command(cmd).await,
                ActorMsg::Int(int) => self.on_internal(int).await,
            }
            self.publish_status();
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.peers.clear();
        done
    }

    fn me(&self) -> DeviceId {
        self.secrets.identity.device_id()
    }

    fn emit(&self, ev: Event) {
        tracing::info!(?ev);
        let _ = self.events_tx.send(ev);
    }

    fn warn(&self, message: String) {
        tracing::warn!("{message}");
        let _ = self.events_tx.send(Event::Warning { message });
    }

    fn member_name(&self, id: &DeviceId) -> String {
        if *id == self.me() {
            return self.cfg.device_name.clone();
        }
        self.group
            .as_ref()
            .and_then(|g| g.membership.get_any(id))
            .map(|m| m.name.clone())
            .unwrap_or_else(|| id.short())
    }

    // ---------------------------------------------------------------- commands

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::CreateGroup {
                name,
                passphrase,
                reply,
            } => {
                let _ = reply.send(self.create_group(name, passphrase).await);
            }
            Command::Join {
                target,
                passphrase,
                reply,
            } => self.start_join(target, passphrase, reply),
            Command::RotatePassphrase { passphrase, reply } => {
                let _ = reply.send(self.rotate_passphrase(passphrase).await);
            }
            Command::Leave { reply } => {
                self.leave_group(true);
                let _ = reply.send(Ok(()));
            }
            Command::RemoveDevice { id, reply } => {
                let res = if id == self.me() {
                    self.leave_group(true);
                    Ok(())
                } else if self.group.is_none() {
                    Err(anyhow!("not in a group"))
                } else {
                    self.update_membership(None, |m| {
                        m.remove(id);
                        true
                    });
                    Ok(())
                };
                let _ = reply.send(res);
            }
            Command::SetPaused(paused) => {
                if self.paused != paused {
                    self.paused = paused;
                    let _ = self.store.set_paused(paused);
                    // Catch up with whatever was copied elsewhere while paused.
                    if !paused && let Some(cur) = self.history.current() {
                        self.write_clipboard(cur.content.clone());
                    }
                }
            }
            Command::Recopy { id, reply } => {
                let res = match self.history.get(&id) {
                    Some(e) => {
                        let content = e.content.clone();
                        self.write_clipboard(content.clone());
                        self.commit_local(content);
                        Ok(())
                    }
                    None => Err(anyhow!("entry is no longer in the history")),
                };
                let _ = reply.send(res);
            }
            Command::AddPeer(addr) => {
                self.static_peers.insert(addr);
                self.backoff.remove(&addr);
                self.dial_missing();
            }
            Command::Shutdown(_) => unreachable!("handled in run"),
        }
    }

    async fn create_group(&mut self, name: String, passphrase: String) -> Result<()> {
        if let Some(g) = &self.group {
            bail!("already in group \"{}\"; leave it first", g.info.name);
        }
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| anyhow!("no randomness: {e}"))?;
        let group_id = GroupId(id);
        let password_key =
            tokio::task::spawn_blocking(move || enroll::password_key(&passphrase, group_id))
                .await??;
        let mut membership = Membership::new();
        membership.add(self.self_member(self.me()));
        let version = self.new_key_version();
        self.set_group(Group {
            info: GroupInfo {
                id: group_id,
                name: name.clone(),
            },
            key: GroupKey {
                version,
                password_key,
            },
            membership,
        })?;
        self.emit(Event::GroupJoined { name });
        Ok(())
    }

    fn self_member(&self, added_by: DeviceId) -> Member {
        Member {
            device_id: self.me(),
            public_key: self.secrets.identity.public_key,
            name: self.cfg.device_name.clone(),
            added_by,
            added_at_ms: now_ms(),
        }
    }

    fn set_group(&mut self, group: Group) -> Result<()> {
        self.store.set_group(Some(&group.info))?;
        self.store.set_membership(&group.membership)?;
        self.save_key(&group.key)?;
        self.group = Some(group);
        self.advertise();
        Ok(())
    }

    fn new_key_version(&mut self) -> KeyVersion {
        KeyVersion {
            hlc: self.clock.now(now_ms()),
            by: self.me(),
        }
    }

    fn save_key(&mut self, key: &GroupKey) -> Result<()> {
        self.store.set_key_version(key.version)?;
        self.secrets.password_key = Some(key.password_key);
        self.secret_store.save(&self.secrets)
    }

    /// Changes the group passphrase and gossips the new key to every member.
    async fn rotate_passphrase(&mut self, passphrase: String) -> Result<()> {
        let Some(group_id) = self.group.as_ref().map(|g| g.info.id) else {
            bail!("not in a group");
        };
        let password_key =
            tokio::task::spawn_blocking(move || enroll::password_key(&passphrase, group_id))
                .await??;
        let key = GroupKey {
            version: self.new_key_version(),
            password_key,
        };
        if !self.adopt_key(key, None) {
            bail!("a newer passphrase change arrived meanwhile");
        }
        Ok(())
    }

    /// Takes `key` if it is newer than ours: persists it, gossips it (except back to
    /// `from`) and reports it. Returns whether it was adopted.
    fn adopt_key(&mut self, key: GroupKey, from: Option<DeviceId>) -> bool {
        let Some(g) = &mut self.group else {
            return false;
        };
        if key.version <= g.key.version {
            return false;
        }
        g.key = key.clone();
        if let Err(e) = self.save_key(&key) {
            self.warn(format!("saving the new group key: {e:#}"));
        }
        self.clock.observe(key.version.hlc, now_ms());
        self.emit(Event::PassphraseChanged {
            by: self.member_name(&key.version.by),
        });
        self.broadcast(encode(&Msg::GroupKey(key)), from);
        true
    }

    fn start_join(&mut self, target: JoinTarget, passphrase: String, reply: Reply) {
        if let Some(g) = &self.group {
            let _ = reply.send(Err(anyhow!(
                "already in group \"{}\"; leave it first",
                g.info.name
            )));
            return;
        }
        let addrs: Vec<SocketAddr> = match target {
            JoinTarget::Addr(a) => vec![a],
            JoinTarget::Group(gid) => self
                .discovered
                .values()
                .filter(|d| d.group.as_ref().is_some_and(|(g, _)| *g == gid))
                .flat_map(|d| d.addrs.iter().copied())
                .collect(),
        };
        if addrs.is_empty() {
            let _ = reply.send(Err(anyhow!("no device of that group is reachable")));
            return;
        }
        let identity = self.secrets.identity.clone();
        let info = JoinInfo {
            name: self.cfg.device_name.clone(),
            listen_port: self.listen_port,
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut result = Err(JoinError::Other(anyhow!("no address tried")));
            for addr in addrs {
                result = enroll::join(addr, &identity, &passphrase, info.clone())
                    .await
                    .map(|(welcome, pk)| (welcome, pk, addr));
                match &result {
                    Ok(_) | Err(JoinError::WrongPassphrase) | Err(JoinError::Refused(_)) => break,
                    Err(JoinError::Other(e)) => tracing::debug!("join via {addr} failed: {e:#}"),
                }
            }
            let _ = tx
                .send(ActorMsg::Int(Internal::JoinFinished { result, reply }))
                .await;
        });
    }

    fn on_welcome(
        &mut self,
        welcome: Welcome,
        member_pk: [u8; 32],
        addr: SocketAddr,
    ) -> Result<()> {
        if self.group.is_some() {
            bail!("already in a group");
        }
        if !welcome.membership.is_member(&self.me()) {
            bail!("the member did not admit this device");
        }
        let name = welcome.group_name.clone();
        self.remember_addr(DeviceId::from_public_key(&member_pk), addr);
        for (id, a) in &welcome.addr_hints {
            self.remember_addr(*id, *a);
        }
        self.history.clear();
        let _ = self.store.clear_history();
        self.set_group(Group {
            info: GroupInfo {
                id: welcome.group_id,
                name: welcome.group_name,
            },
            key: welcome.key,
            membership: welcome.membership,
        })?;
        self.emit(Event::GroupJoined { name });
        self.dial_missing();
        Ok(())
    }

    /// Leaves the group. With `announce`, tells the others first (tombstone gossip).
    /// A fresh identity is generated so this PC can join again later.
    fn leave_group(&mut self, announce: bool) {
        let me = self.me();
        let Some(mut group) = self.group.take() else {
            return;
        };
        if announce {
            group.membership.remove(me);
            let bytes = encode(&Msg::Membership(group.membership.clone()));
            for p in self.peers.values() {
                let _ = p.tx.try_send(bytes.clone());
            }
        }
        // Dropping the senders lets writer tasks flush the gossip, then close.
        self.peers.clear();
        self.dialing.clear();
        self.backoff.clear();
        self.addr_book.clear();
        self.history.clear();
        let res = (|| -> Result<()> {
            self.store.set_group(None)?;
            self.store.set_membership(&Membership::new())?;
            self.store.set_addr_book(&[])?;
            self.store.clear_history()?;
            self.secrets = Secrets {
                identity: Identity::generate()?,
                password_key: None,
            };
            self.secret_store.save(&self.secrets)?;
            *self.identity.write().unwrap() = self.secrets.identity.clone();
            Ok(())
        })();
        if let Err(e) = res {
            self.warn(format!("leaving group: {e:#}"));
        }
        self.advertise();
        self.emit(Event::GroupLeft);
    }

    // ---------------------------------------------------------------- internal

    async fn on_internal(&mut self, int: Internal) {
        match int {
            Internal::LocalCopy {
                content,
                writes_seen,
            } => self.on_local_copy(content, writes_seen),
            Internal::Session {
                session,
                outbound,
                dialed,
            } => self.on_session(session, outbound, dialed),
            Internal::Inbound { peer, conn, msg } => {
                if self.peers.get(&peer).is_some_and(|p| p.conn == conn) {
                    self.on_message(peer, msg);
                }
            }
            Internal::Closed { peer, conn } => {
                if self.peers.get(&peer).is_some_and(|p| p.conn == conn) {
                    self.peers.remove(&peer);
                    self.emit(Event::PeerDisconnected {
                        name: self.member_name(&peer),
                    });
                }
            }
            Internal::DialFailed { addr } => {
                self.dialing.remove(&addr);
                let now = Instant::now();
                let e = self
                    .backoff
                    .entry(addr)
                    .or_insert((now, Duration::from_secs(1)));
                e.1 = (e.1 * 2).min(MAX_BACKOFF);
                e.0 = now + e.1;
            }
            Internal::JoinAuthority(reply) => {
                let _ = reply.send(self.group.as_ref().map(|g| JoinAuthority {
                    group_id: g.info.id,
                    group_name: g.info.name.clone(),
                    password_key: g.key.password_key,
                }));
            }
            Internal::Admit {
                public_key,
                info,
                ip,
                reply,
            } => {
                let _ = reply.send(self.admit(public_key, info, ip));
            }
            Internal::JoinFinished { result, reply } => {
                let res = match result {
                    Ok((welcome, pk, addr)) => self.on_welcome(welcome, pk, addr),
                    Err(e) => Err(e.into()),
                };
                let _ = reply.send(res);
            }
            Internal::JoinRejected(addr) => {
                self.emit(Event::JoinRejected {
                    from_addr: addr.to_string(),
                });
            }
            Internal::Discovery(ev) => self.on_discovery(ev),
            Internal::Tick => {
                self.lan_ip = crate::lan_ip(); // Wi-Fi changes, DHCP renewals
                self.dial_missing();
            }
        }
    }

    fn write_clipboard(&mut self, content: Content) {
        self.clip_writes += 1;
        self.clip.write(content);
    }

    fn on_local_copy(&mut self, content: Content, writes_seen: u64) {
        if self.paused {
            return;
        }
        if content.size() > self.cfg.max_entry_bytes {
            self.warn(format!(
                "not syncing {} of {} KB (limit {} KB)",
                content.kind(),
                content.size() / 1024,
                self.cfg.max_entry_bytes / 1024
            ));
            return;
        }
        if self
            .history
            .current()
            .is_some_and(|e| e.content.hash() == content.hash())
        {
            return;
        }
        self.emit(Event::LocalCopy {
            preview: content.preview(40),
        });
        // A remote entry was written to the OS clipboard after the user copied this, but
        // the copy is newer by HLC order: put it back so this PC agrees with the others.
        if writes_seen < self.clip_writes {
            self.write_clipboard(content.clone());
        }
        self.commit_local(content);
    }

    fn commit_local(&mut self, content: Content) {
        let entry = Entry {
            id: EntryId {
                hlc: self.clock.now(now_ms()),
                origin: self.me(),
            },
            content,
        };
        self.insert_and_flood(entry, None);
    }

    /// Stores a new entry and forwards it to every peer except `from`.
    /// Returns `Some(is_current)` if the entry was new.
    fn insert_and_flood(&mut self, entry: Entry, from: Option<DeviceId>) -> Option<bool> {
        let InsertOutcome::New { is_current } = self.history.insert(entry.clone()) else {
            return None;
        };
        let keep: BTreeSet<EntryId> = self.history.ids().copied().collect();
        if let Err(e) = self.store.put_entry(&entry, &keep) {
            tracing::warn!("persisting history: {e:#}");
        }
        if self.group.is_some() {
            let bytes = encode(&Msg::Entry(entry));
            self.broadcast(bytes, from);
        }
        Some(is_current)
    }

    fn broadcast(&mut self, bytes: Arc<Vec<u8>>, except: Option<DeviceId>) {
        let ids: Vec<DeviceId> = self
            .peers
            .keys()
            .copied()
            .filter(|id| Some(*id) != except)
            .collect();
        for id in ids {
            self.send_to(id, bytes.clone());
        }
    }

    fn send_to(&mut self, id: DeviceId, bytes: Arc<Vec<u8>>) {
        let Some(p) = self.peers.get(&id) else { return };
        if p.tx.try_send(bytes).is_err() {
            // Peer is too slow or gone; drop it. It catches up on reconnect.
            tracing::warn!("send queue to {} full; dropping connection", id.short());
            self.peers.remove(&id);
        }
    }

    fn on_session(&mut self, session: Session, outbound: bool, dialed: Option<SocketAddr>) {
        if let Some(a) = dialed {
            self.dialing.remove(&a);
        }
        let remote = DeviceId::from_public_key(&session.remote_public_key);
        let me = self.me();
        let Some(group) = &self.group else { return };
        let known = group
            .membership
            .get(&remote)
            .is_some_and(|m| m.public_key == session.remote_public_key);
        if !known || remote == me {
            tracing::debug!("rejecting session from non-member {}", remote.short());
            return;
        }
        if let Some(a) = dialed {
            self.backoff.remove(&a);
            self.remember_addr(remote, a);
        }
        let dialer = if outbound { me } else { remote };
        let replacing = if let Some(existing) = self.peers.get(&remote) {
            // Both sides keep the connection dialed by the smaller device id.
            let preferred = me.min(remote);
            if existing.dialer == preferred || dialer != preferred {
                return;
            }
            true
        } else {
            false
        };

        self.next_conn += 1;
        let conn = self.next_conn;
        let addr = session.peer_addr;
        let Session {
            mut reader,
            mut writer,
            ..
        } = session;
        reader.set_max_message(self.cfg.max_entry_bytes + 1024 * 1024);
        let (ptx, mut prx) = mpsc::channel::<Arc<Vec<u8>>>(PEER_QUEUE);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    m = prx.recv() => match m {
                        Some(bytes) => if writer.send_bytes(&bytes).await.is_err() { break },
                        None => break,
                    },
                    _ = tokio::time::sleep(KEEPALIVE) => if writer.send(&Msg::Ping).await.is_err() { break },
                }
            }
            writer.shutdown().await;
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(IDLE_TIMEOUT, reader.recv::<Msg>()).await {
                    Ok(Ok(Msg::Ping)) => {}
                    Ok(Ok(msg)) => {
                        if tx
                            .send(ActorMsg::Int(Internal::Inbound {
                                peer: remote,
                                conn,
                                msg,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!("connection to {} closed: {e:#}", remote.short());
                        break;
                    }
                    Err(_) => {
                        tracing::debug!("connection to {} idle; closing", remote.short());
                        break;
                    }
                }
            }
            let _ = tx
                .send(ActorMsg::Int(Internal::Closed { peer: remote, conn }))
                .await;
        });

        self.peers.insert(
            remote,
            Peer {
                conn,
                tx: ptx,
                dialer,
                addr,
            },
        );
        let hello = Msg::Hello(Hello {
            name: self.cfg.device_name.clone(),
            listen_port: self.listen_port,
            membership: self
                .group
                .as_ref()
                .map(|g| g.membership.clone())
                .unwrap_or_default(),
            head: self.history.current_id(),
            key_version: self
                .group
                .as_ref()
                .map(|g| g.key.version)
                .unwrap_or_default(),
        });
        self.send_to(remote, encode(&hello));
        if !replacing {
            self.emit(Event::PeerConnected {
                name: self.member_name(&remote),
            });
        }
    }

    fn on_message(&mut self, peer: DeviceId, msg: Msg) {
        match msg {
            Msg::Hello(hello) => {
                if let Some(ip) = self.peers.get(&peer).map(|p| p.addr.ip()) {
                    self.remember_addr(peer, SocketAddr::new(ip, hello.listen_port));
                }
                self.update_membership(Some(peer), |m| m.merge(&hello.membership));
                // Catch-up: if we have something newer than the peer, send it.
                if let Some(cur) = self.history.current()
                    && hello.head.is_none_or(|head| cur.id > head)
                {
                    let bytes = encode(&Msg::Entry(cur.clone()));
                    self.send_to(peer, bytes);
                }
                // A peer that missed a passphrase change (offline, or joined via a stale
                // member) gets the newer key.
                if let Some(g) = &self.group
                    && g.key.version > hello.key_version
                {
                    let bytes = encode(&Msg::GroupKey(g.key.clone()));
                    self.send_to(peer, bytes);
                }
            }
            Msg::Entry(entry) => self.on_remote_entry(peer, entry),
            Msg::Membership(m) => self.update_membership(Some(peer), |mine| mine.merge(&m)),
            Msg::GroupKey(key) => {
                self.adopt_key(key, Some(peer));
            }
            Msg::Ping => {}
        }
    }

    fn on_remote_entry(&mut self, from: DeviceId, entry: Entry) {
        let Some(g) = &self.group else { return };
        if !g.membership.ever_added(&entry.id.origin) {
            tracing::warn!(
                "dropping entry from unknown origin {}",
                entry.id.origin.short()
            );
            return;
        }
        if entry.content.size() > self.cfg.max_entry_bytes {
            return;
        }
        self.clock.observe(entry.id.hlc, now_ms());
        let origin = entry.id.origin;
        let content = entry.content.clone();
        if self.insert_and_flood(entry, Some(from)) == Some(true) && !self.paused {
            self.emit(Event::RemoteApplied {
                from: self.member_name(&origin),
                preview: content.preview(40),
            });
            self.write_clipboard(content);
        }
    }

    /// Applies a membership mutation; on change persists it, gossips it (except back to
    /// `from`), drops removed peers and reports joins/removals.
    fn update_membership(
        &mut self,
        from: Option<DeviceId>,
        f: impl FnOnce(&mut Membership) -> bool,
    ) {
        let me = self.me();
        let Some(g) = &mut self.group else { return };
        let before: BTreeSet<DeviceId> = g.membership.live().map(|m| m.device_id).collect();
        if !f(&mut g.membership) {
            return;
        }
        let after: BTreeSet<DeviceId> = g.membership.live().map(|m| m.device_id).collect();
        let membership = g.membership.clone();
        if let Err(e) = self.store.set_membership(&membership) {
            tracing::warn!("persisting membership: {e:#}");
        }
        if !membership.is_member(&me) {
            // Someone removed us.
            self.leave_group(false);
            return;
        }
        // Gossip first, so removed devices also learn about their removal: dropping their
        // sender below lets the writer flush this message before closing.
        self.broadcast(encode(&Msg::Membership(membership)), from);
        for id in after.difference(&before) {
            self.emit(Event::MemberJoined {
                name: self.member_name(id),
            });
        }
        for id in before.difference(&after) {
            self.emit(Event::MemberRemoved {
                name: self.member_name(id),
            });
            self.peers.remove(id);
        }
    }

    fn admit(&mut self, public_key: [u8; 32], info: JoinInfo, ip: IpAddr) -> Result<Welcome> {
        let me = self.me();
        let Some(g) = &self.group else {
            bail!("not in a group")
        };
        let id = DeviceId::from_public_key(&public_key);
        if g.membership.was_removed(&id) {
            bail!("this device was removed from the group; it must leave and join again");
        }
        let member = Member {
            device_id: id,
            public_key,
            name: info.name.clone(),
            added_by: me,
            added_at_ms: now_ms(),
        };
        self.update_membership(None, |m| m.add(member));
        self.remember_addr(id, SocketAddr::new(ip, info.listen_port));
        let g = self.group.as_ref().expect("still in group");
        let addr_hints = self
            .addr_book
            .iter()
            .filter(|(d, _)| **d != id && **d != me)
            .flat_map(|(d, addrs)| addrs.iter().map(|a| (*d, *a)))
            .collect();
        Ok(Welcome {
            group_id: g.info.id,
            group_name: g.info.name.clone(),
            key: g.key.clone(),
            membership: g.membership.clone(),
            addr_hints,
        })
    }

    fn on_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Found(d) => {
                let ours = match (&self.group, &d.group) {
                    (Some(g), Some((gid, _))) => *gid == g.info.id,
                    _ => false,
                };
                if ours {
                    for a in d.addrs.clone() {
                        self.remember_addr(d.device_id, a);
                    }
                }
                self.discovered.insert(d.device_id, d);
                if ours {
                    self.dial_missing();
                }
            }
            DiscoveryEvent::Lost(id) => {
                self.discovered.remove(&id);
            }
        }
    }

    fn remember_addr(&mut self, id: DeviceId, addr: SocketAddr) {
        if addr.port() == 0 || id == self.me() {
            return;
        }
        let set = self.addr_book.entry(id).or_default();
        if set.contains(&addr) {
            return;
        }
        // Keep the book small; newer addresses win.
        while set.len() >= 6 {
            set.pop_first();
        }
        set.insert(addr);
        let flat: Vec<(DeviceId, BTreeSet<SocketAddr>)> = self
            .addr_book
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        if let Err(e) = self.store.set_addr_book(&flat) {
            tracing::warn!("persisting addresses: {e:#}");
        }
    }

    /// Dials members we are not connected to, plus configured static peers.
    fn dial_missing(&mut self) {
        let Some(g) = &self.group else { return };
        let me = self.me();
        let now = Instant::now();
        let mut targets: BTreeSet<SocketAddr> = BTreeSet::new();
        for m in g.membership.live() {
            if m.device_id == me || self.peers.contains_key(&m.device_id) {
                continue;
            }
            if let Some(addrs) = self.addr_book.get(&m.device_id) {
                targets.extend(addrs.iter().copied());
            }
        }
        for a in &self.static_peers {
            let connected = self
                .addr_book
                .iter()
                .any(|(id, addrs)| self.peers.contains_key(id) && addrs.contains(a));
            if !connected {
                targets.insert(*a);
            }
        }
        let group_id = g.info.id;
        for addr in targets {
            if self.dialing.contains(&addr)
                || self.backoff.get(&addr).is_some_and(|(t, _)| *t > now)
            {
                continue;
            }
            self.dialing.insert(addr);
            let identity = self.secrets.identity.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let msg = match session::connect_sync(addr, &identity, group_id).await {
                    Ok(session) => Internal::Session {
                        session,
                        outbound: true,
                        dialed: Some(addr),
                    },
                    Err(e) => {
                        tracing::debug!("dial {addr} failed: {e:#}");
                        Internal::DialFailed { addr }
                    }
                };
                let _ = tx.send(ActorMsg::Int(msg)).await;
            });
        }
    }

    fn advert(&self) -> Advert {
        Advert {
            device_id: self.me(),
            name: self.cfg.device_name.clone(),
            group: self
                .group
                .as_ref()
                .map(|g| (g.info.id, g.info.name.clone())),
            port: self.listen_port,
        }
    }

    fn advertise(&mut self) {
        let advert = self.advert();
        if let Some(d) = &mut self.discovery
            && let Err(e) = d.advertise(&advert)
        {
            tracing::warn!("mDNS advertise failed: {e:#}");
        }
    }

    // ---------------------------------------------------------------- status

    fn publish_status(&self) {
        let status = self.build_status();
        self.status_tx.send_if_modified(|s| {
            if *s == status {
                false
            } else {
                *s = status;
                true
            }
        });
    }

    fn build_status(&self) -> Status {
        let me = self.me();
        let (group, members) = match &self.group {
            Some(g) => (
                Some(GroupStatus {
                    id: g.info.id.to_string(),
                    name: g.info.name.clone(),
                    passphrase_changed_ms: g.key.version.hlc.wall_ms,
                    passphrase_changed_by: self.member_name(&g.key.version.by),
                }),
                g.membership
                    .live()
                    .map(|m| MemberStatus {
                        device_id: m.device_id.to_string(),
                        name: m.name.clone(),
                        online: m.device_id == me || self.peers.contains_key(&m.device_id),
                        is_self: m.device_id == me,
                    })
                    .collect(),
            ),
            None => (None, Vec::new()),
        };
        let history = self
            .history
            .iter_newest()
            .take(STATUS_HISTORY)
            .map(|e| HistoryItem {
                id: e.id.to_hex(),
                kind: e.content.kind().to_string(),
                preview: e.content.preview(48),
                size: e.content.size(),
                origin: self.member_name(&e.id.origin),
                at_ms: e.id.hlc.wall_ms,
            })
            .collect();
        let our_gid = self.group.as_ref().map(|g| g.info.id);
        let mut groups: BTreeMap<GroupId, DiscoveredGroup> = BTreeMap::new();
        for d in self.discovered.values() {
            let Some((gid, gname)) = &d.group else {
                continue;
            };
            if Some(*gid) == our_gid {
                continue;
            }
            let entry = groups.entry(*gid).or_insert_with(|| DiscoveredGroup {
                group_id: gid.to_string(),
                name: gname.clone(),
                devices: Vec::new(),
            });
            entry.devices.push(d.name.clone());
            entry.devices.sort();
        }
        Status {
            device_id: me.to_string(),
            device_name: self.cfg.device_name.clone(),
            listen_port: self.listen_port,
            lan_ip: self.lan_ip.map(|ip| ip.to_string()),
            group,
            members,
            paused: self.paused,
            history,
            discovered_groups: groups.into_values().collect(),
        }
    }
}

// -------------------------------------------------------------------- listener

async fn listen(
    listener: TcpListener,
    identity: Arc<RwLock<Identity>>,
    tx: mpsc::Sender<ActorMsg>,
) {
    let limiter = Arc::new(Mutex::new(RateLimiter::new(5)));
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let identity = identity.read().unwrap().clone();
        let tx = tx.clone();
        let limiter = limiter.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_incoming(stream, addr, identity, tx, limiter).await {
                tracing::debug!("incoming connection from {addr}: {e:#}");
            }
        });
    }
}

async fn handle_incoming(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    identity: Identity,
    tx: mpsc::Sender<ActorMsg>,
    limiter: Arc<Mutex<RateLimiter>>,
) -> Result<()> {
    let incoming = Incoming::read(stream).await?;
    match incoming.preamble {
        Preamble::Sync { proto, .. } => {
            if proto != PROTOCOL_VERSION {
                bail!("protocol version {proto} not supported");
            }
            let session = incoming.accept_sync(&identity).await?;
            tx.send(ActorMsg::Int(Internal::Session {
                session,
                outbound: false,
                dialed: None,
            }))
            .await
            .map_err(|_| anyhow!("node stopped"))?;
        }
        Preamble::Join { proto } => {
            if proto != PROTOCOL_VERSION {
                bail!("protocol version {proto} not supported");
            }
            if !limiter.lock().unwrap().allow(addr.ip(), Instant::now()) {
                bail!("join attempts from {} rate limited", addr.ip());
            }
            let (atx, arx) = oneshot::channel();
            tx.send(ActorMsg::Int(Internal::JoinAuthority(atx)))
                .await
                .map_err(|_| anyhow!("node stopped"))?;
            let authority = arx.await?;
            let admit_tx = tx.clone();
            let res = enroll::serve_join(
                incoming,
                &identity,
                authority,
                async move |public_key, info| {
                    let (rtx, rrx) = oneshot::channel();
                    admit_tx
                        .send(ActorMsg::Int(Internal::Admit {
                            public_key,
                            info,
                            ip: addr.ip(),
                            reply: rtx,
                        }))
                        .await
                        .map_err(|_| anyhow!("node stopped"))?;
                    rrx.await?
                },
            )
            .await;
            match res {
                Ok(()) => limiter.lock().unwrap().record_success(addr.ip()),
                Err(JoinError::WrongPassphrase) => {
                    limiter
                        .lock()
                        .unwrap()
                        .record_failure(addr.ip(), Instant::now());
                    let _ = tx.send(ActorMsg::Int(Internal::JoinRejected(addr))).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
