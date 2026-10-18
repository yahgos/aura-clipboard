//! `aura`: headless runner and debug tool for Aura Clipboard.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aura_core::GroupId;
use aura_net::discovery::{Discovery, DiscoveryEvent};
use aura_node::{Config, Event, JoinTarget, Node, NodeHandle, Status, default_data_dir};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "aura",
    version,
    about = "Shared clipboard for the computers on your LAN"
)]
struct Cli {
    /// Data directory (config, state, secrets). Use different ones to run several nodes on one PC.
    #[arg(long, global = true, env = "AURA_DATA_DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the clipboard node in the foreground.
    Run {
        /// Device name shown to other PCs (overrides config.toml).
        #[arg(long)]
        name: Option<String>,
        /// Port to listen on (overrides config.toml; 0 = any).
        #[arg(long)]
        port: Option<u16>,
        /// Extra peer to dial (repeatable), for networks without multicast.
        #[arg(long = "peer")]
        peers: Vec<SocketAddr>,
        /// Disable mDNS discovery.
        #[arg(long)]
        no_discovery: bool,
        /// Never serve the phone page from this PC.
        #[arg(long)]
        no_web: bool,
        /// Create a new group with this name before running.
        #[arg(long, conflicts_with_all = ["join", "join_group"])]
        create_group: Option<String>,
        /// Join the group of the device at this address (host or IP, port optional).
        #[arg(long, conflicts_with = "join_group")]
        join: Option<String>,
        /// Join a group found by `aura discover` (group id).
        #[arg(long)]
        join_group: Option<GroupId>,
        /// Leave the current group before running.
        #[arg(long)]
        leave: bool,
        /// Change the group passphrase (prompted) for every member, then run.
        #[arg(long)]
        change_passphrase: bool,
    },
    /// List Aura groups and devices on the local network.
    Discover {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Print local clipboard changes (no networking); handy to test OS support.
    Watch,
}

fn passphrase(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("AURA_PASSPHRASE") {
        return Ok(p);
    }
    let p = rpassword::prompt_password("Group passphrase: ")?;
    if confirm && rpassword::prompt_password("Repeat passphrase: ")? != p {
        bail!("passphrases do not match");
    }
    Ok(p)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aura=info,aura_node=info,warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);

    match cli.cmd {
        Cmd::Run {
            name,
            port,
            peers,
            no_discovery,
            no_web,
            create_group,
            join,
            join_group,
            leave,
            change_passphrase,
        } => {
            let mut cfg = Config::load_or_init(&data_dir)?;
            if let Some(n) = name {
                cfg.device_name = n;
            }
            if let Some(p) = port {
                cfg.port = p;
            }
            cfg.static_peers.extend(peers);
            cfg.discovery &= !no_discovery;
            cfg.web_enabled &= !no_web;
            println!("data dir: {}", data_dir.display());
            let node = Node::start(&data_dir, cfg, aura_node::system_clipboard()).await?;

            if leave {
                node.leave().await?;
            }
            if change_passphrase {
                println!("enter the NEW group passphrase");
                node.rotate_passphrase(&passphrase(true)?).await?;
            }
            if let Some(group) = create_group {
                node.create_group(&group, &passphrase(true)?).await?;
            } else if let Some(addr) = join {
                let addr = aura_node::resolve_peer(&addr).await?;
                node.join(JoinTarget::Addr(addr), &passphrase(false)?)
                    .await?;
            } else if let Some(gid) = join_group {
                // Give mDNS a moment to find the group's devices.
                tokio::time::sleep(Duration::from_secs(2)).await;
                node.join(JoinTarget::Group(gid), &passphrase(false)?)
                    .await?;
            }
            // Serves the phone page whenever this PC is elected to.
            let _web = aura_web::spawn(node.clone());
            run(node).await
        }
        Cmd::Discover { seconds } => discover(seconds).await,
        Cmd::Watch => watch().await,
    }
}

async fn run(node: NodeHandle) -> Result<()> {
    print_status(&node.status());
    let mut status = node.watch_status();
    let mut events = node.events();
    let mut last_summary = summary(&node.status());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            ev = events.recv() => match ev {
                Ok(ev) => print_event(&ev, &node.status()),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            changed = status.changed() => {
                if changed.is_err() { break }
                let s = status.borrow_and_update().clone();
                let sum = summary(&s);
                if sum != last_summary {
                    println!("{sum}");
                    last_summary = sum;
                }
            }
        }
    }
    println!("shutting down…");
    node.shutdown().await;
    Ok(())
}

fn summary(s: &Status) -> String {
    match &s.group {
        Some(g) => {
            let online: Vec<&str> = s
                .members
                .iter()
                .filter(|m| m.online && !m.is_self)
                .map(|m| m.name.as_str())
                .collect();
            format!(
                "● group \"{}\": {} of {} other device(s) online{}{}",
                g.name,
                online.len(),
                s.members.len().saturating_sub(1),
                if online.is_empty() {
                    String::new()
                } else {
                    format!(": {}", online.join(", "))
                },
                if s.paused { " (paused)" } else { "" }
            )
        }
        None => {
            let found: Vec<String> = s
                .discovered_groups
                .iter()
                .map(|g| format!("\"{}\" ({}) [{}]", g.name, g.group_id, g.devices.join(", ")))
                .collect();
            if found.is_empty() {
                "○ not in a group; use --create-group NAME or --join HOST:PORT".to_string()
            } else {
                format!("○ not in a group; groups nearby: {}", found.join("; "))
            }
        }
    }
}

fn print_status(s: &Status) {
    println!(
        "device {} ({}) on port {}",
        s.device_name,
        &s.device_id[..8],
        s.listen_port
    );
    println!("{}", summary(s));
}

fn print_event(ev: &Event, status: &Status) {
    match ev {
        Event::LocalCopy { preview } => println!("⇡ copied: {preview}"),
        Event::RemoteApplied { from, preview } => println!("⇣ from {from}: {preview}"),
        Event::PeerConnected { name } => println!("+ {name} connected"),
        Event::PeerDisconnected { name } => println!("- {name} disconnected"),
        Event::MemberJoined { name } => println!("★ {name} joined the group"),
        Event::MemberRemoved { name } => println!("✕ {name} was removed from the group"),
        Event::GroupJoined { name } => println!("✓ joined group \"{name}\""),
        Event::GroupLeft => println!("left the group"),
        Event::PassphraseChanged { by } => println!("🔑 group passphrase changed by {by}"),
        Event::PhonePage { serving: true } => {
            let port = status.web_port;
            println!(
                "📱 this PC now serves the phone page: https://{}:{port}",
                aura_web::WEB_HOST
            );
            if let Some(ip) = &status.lan_ip {
                println!("   (or https://{ip}:{port} if your phone can't open aura.local)");
            }
        }
        Event::PhonePage { serving: false } => {
            println!("📱 another PC serves the phone page now");
        }
        Event::JoinRejected { from_addr } => {
            println!("! rejected join attempt from {from_addr} (wrong passphrase)")
        }
        Event::Warning { message } => println!("! {message}"),
    }
}

async fn discover(seconds: u64) -> Result<()> {
    let (_d, mut rx) = Discovery::start(None).context("starting mDNS")?;
    println!("browsing for {seconds}s…");
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut found = 0;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = rx.recv() => match ev {
                Some(DiscoveryEvent::Found(d)) => {
                    found += 1;
                    let group = match &d.group {
                        Some((gid, name)) => format!("group \"{name}\" ({gid})"),
                        None => "no group".into(),
                    };
                    let addrs: Vec<String> = d.addrs.iter().map(|a| a.to_string()).collect();
                    println!("{:<20} {group:<55} {}", d.name, addrs.join(" "));
                }
                Some(_) => {}
                None => break,
            }
        }
    }
    if found == 0 {
        println!("no Aura devices found (is multicast blocked on this network?)");
    }
    Ok(())
}

async fn watch() -> Result<()> {
    let _worker = aura_clip::ClipboardWorker::spawn(
        || Ok(Box::new(aura_clip::SystemClipboard::new()?) as Box<dyn aura_clip::ClipboardBackend>),
        Duration::from_millis(250),
        |content, _| {
            println!(
                "{:>5}  {:>8} B  {}",
                content.kind(),
                content.size(),
                content.preview(70)
            )
        },
    )?;
    println!("watching the clipboard; copy something (Ctrl-C to stop)");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
