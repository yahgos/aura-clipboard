//! Local control channel, so the `aura` CLI can talk to a running Aura (tray app
//! or `aura run`) on the same PC.
//!
//! The running node listens on `127.0.0.1:<random port>` and writes the port and
//! a random token to `control.json` in its data directory, readable only by the
//! user. Every request must carry the token. Requests and responses are one JSON
//! object per line.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::{EntryId, NodeHandle, SearchResult};

const FILE: &str = "control.json";
const MAX_REQUEST: u64 = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct ControlFile {
    port: u16,
    token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Search { query: String, limit: usize },
    Recopy { id: String },
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    results: Option<Vec<SearchResult>>,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn tokens_match(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Serves the control channel until the node stops, then removes `control.json`.
pub(crate) async fn serve(node: NodeHandle, data_dir: PathBuf) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("no randomness: {e}"))?;
    let token = hex::encode(raw);
    let file = ControlFile {
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    crate::secrets::write_private(&path(&data_dir), &serde_json::to_vec(&file)?)?;

    let mut status = node.watch_status();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let (node, token) = (node.clone(), token.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, &node, &token).await {
                        tracing::debug!("control request failed: {e:#}");
                    }
                });
            }
            // The status channel closes when the node shuts down.
            changed = status.changed() => if changed.is_err() { break },
        }
    }
    let _ = std::fs::remove_file(path(&data_dir));
    Ok(())
}

async fn handle(stream: TcpStream, node: &NodeHandle, token: &str) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(
        TIMEOUT,
        BufReader::new(read.take(MAX_REQUEST)).read_line(&mut line),
    )
    .await??;
    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(env) if tokens_match(&env.token, token) => match run(node, env.request).await {
            Ok(results) => Response {
                ok: true,
                error: None,
                results,
            },
            Err(e) => Response {
                ok: false,
                error: Some(format!("{e:#}")),
                results: None,
            },
        },
        Ok(_) => Response {
            ok: false,
            error: Some("bad token".into()),
            results: None,
        },
        Err(e) => Response {
            ok: false,
            error: Some(format!("bad request: {e}")),
            results: None,
        },
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    Ok(())
}

async fn run(node: &NodeHandle, request: Request) -> Result<Option<Vec<SearchResult>>> {
    match request {
        Request::Search { query, limit } => Ok(Some(node.search(&query, limit.min(500)).await?)),
        Request::Recopy { id } => {
            let id = EntryId::from_hex(&id).context("bad entry id")?;
            node.recopy(id).await?;
            Ok(None)
        }
    }
}

/// Talks to the Aura running on this PC for `data_dir`.
pub struct Client {
    port: u16,
    token: String,
}

impl Client {
    /// `None` if no Aura is running for this data directory.
    pub async fn connect(data_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(path(data_dir)).ok()?;
        let file: ControlFile = serde_json::from_slice(&bytes).ok()?;
        // A file left behind by a crashed instance points at a closed port.
        TcpStream::connect(("127.0.0.1", file.port)).await.ok()?;
        Some(Self {
            port: file.port,
            token: file.token,
        })
    }

    async fn call(&self, request: Request) -> Result<Response> {
        let stream = TcpStream::connect(("127.0.0.1", self.port))
            .await
            .context("connecting to the running Aura")?;
        let (read, mut write) = stream.into_split();
        let mut out = serde_json::to_vec(&Envelope {
            token: self.token.clone(),
            request,
        })?;
        out.push(b'\n');
        write.write_all(&out).await?;
        let mut line = String::new();
        tokio::time::timeout(TIMEOUT, BufReader::new(read).read_line(&mut line)).await??;
        let response: Response = serde_json::from_str(&line).context("bad response")?;
        if !response.ok {
            bail!(response.error.unwrap_or_else(|| "request failed".into()));
        }
        Ok(response)
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let r = self
            .call(Request::Search {
                query: query.into(),
                limit,
            })
            .await?;
        Ok(r.results.unwrap_or_default())
    }

    /// Makes an entry the current clipboard on every PC.
    pub async fn recopy(&self, id: &str) -> Result<()> {
        self.call(Request::Recopy { id: id.into() }).await?;
        Ok(())
    }

    /// Sends a request with a different token (for tests).
    #[doc(hidden)]
    pub fn with_token(mut self, token: &str) -> Self {
        self.token = token.into();
        self
    }
}
