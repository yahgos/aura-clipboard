//! Noise sessions over TCP.
//!
//! Wire format: every frame is `u16 big-endian length ‖ bytes`. During the
//! handshake the bytes are Noise handshake messages; afterwards each frame is a
//! Noise transport message whose plaintext is `flag ‖ chunk`, where `flag = 1`
//! marks the last chunk of an application message. That lets one postcard
//! message exceed Noise's 64 KiB limit (images).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aura_core::{GroupId, PROTOCOL_VERSION};
use serde::Serialize;
use serde::de::DeserializeOwned;
use snow::{HandshakeState, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::Identity;
use crate::identity::NOISE_XX;
use crate::proto::Preamble;

const NOISE_MAX: usize = 65535;
const TAG_LEN: usize = 16;
const MAX_CHUNK: usize = NOISE_MAX - TAG_LEN - 1;
const PROLOGUE_PREFIX: &[u8] = b"aura-clipboard/v1";
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap for one application message (largest image + overhead).
pub const DEFAULT_MAX_MESSAGE: usize = 48 * 1024 * 1024;

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len = u16::try_from(bytes.len()).map_err(|_| anyhow!("frame too large"))?;
    let mut buf = Vec::with_capacity(2 + bytes.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
    w.write_all(&buf).await?;
    Ok(())
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub(crate) async fn write_plain<T: Serialize, W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &T,
) -> Result<()> {
    write_frame(w, &postcard::to_stdvec(msg)?).await
}

pub(crate) async fn read_plain<T: DeserializeOwned, R: AsyncRead + Unpin>(r: &mut R) -> Result<T> {
    Ok(postcard::from_bytes(&read_frame(r).await?)?)
}

pub(crate) fn prologue(preamble: &[u8]) -> Vec<u8> {
    [PROLOGUE_PREFIX, preamble].concat()
}

/// Runs a Noise handshake to completion over `stream`.
pub(crate) async fn run_handshake(
    stream: &mut TcpStream,
    mut hs: HandshakeState,
) -> Result<StatelessTransportState> {
    let mut buf = vec![0u8; NOISE_MAX];
    let mut payload = vec![0u8; NOISE_MAX];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf)?;
            write_frame(stream, &buf[..n]).await?;
        } else {
            let msg = read_frame(stream).await?;
            hs.read_message(&msg, &mut payload)
                .context("noise handshake failed")?;
        }
    }
    Ok(hs.into_stateless_transport_mode()?)
}

/// An authenticated, encrypted connection to another device.
pub struct Session {
    pub remote_public_key: [u8; 32],
    pub peer_addr: SocketAddr,
    pub reader: SessionReader,
    pub writer: SessionWriter,
}

impl Session {
    pub(crate) fn new(stream: TcpStream, transport: StatelessTransportState) -> Result<Self> {
        let remote_public_key: [u8; 32] = transport
            .get_remote_static()
            .ok_or_else(|| anyhow!("peer sent no static key"))?
            .try_into()
            .map_err(|_| anyhow!("bad static key length"))?;
        let peer_addr = stream.peer_addr()?;
        stream.set_nodelay(true)?;
        let (r, w) = stream.into_split();
        let transport = Arc::new(transport);
        Ok(Self {
            remote_public_key,
            peer_addr,
            reader: SessionReader {
                r,
                transport: transport.clone(),
                nonce: 0,
                max_message: DEFAULT_MAX_MESSAGE,
            },
            writer: SessionWriter {
                w,
                transport,
                nonce: 0,
            },
        })
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        self.writer.send(msg).await
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.reader.recv().await
    }
}

pub struct SessionWriter {
    w: OwnedWriteHalf,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
}

impl SessionWriter {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let bytes = postcard::to_stdvec(msg)?;
        self.send_bytes(&bytes).await
    }

    pub async fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let mut plain = Vec::with_capacity(MAX_CHUNK + 1);
        let mut cipher = vec![0u8; NOISE_MAX];
        let mut chunks = bytes.chunks(MAX_CHUNK).peekable();
        if chunks.peek().is_none() {
            bail!("empty message");
        }
        while let Some(chunk) = chunks.next() {
            plain.clear();
            plain.push(u8::from(chunks.peek().is_none()));
            plain.extend_from_slice(chunk);
            let n = self
                .transport
                .write_message(self.nonce, &plain, &mut cipher)?;
            self.nonce += 1;
            write_frame(&mut self.w, &cipher[..n]).await?;
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.w.shutdown().await;
    }
}

pub struct SessionReader {
    r: OwnedReadHalf,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    max_message: usize,
}

impl SessionReader {
    pub fn set_max_message(&mut self, max: usize) {
        self.max_message = max;
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        let bytes = self.recv_bytes().await?;
        Ok(postcard::from_bytes(&bytes)?)
    }

    pub async fn recv_bytes(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut plain = vec![0u8; NOISE_MAX];
        loop {
            let frame = read_frame(&mut self.r).await?;
            let n = self
                .transport
                .read_message(self.nonce, &frame, &mut plain)?;
            self.nonce += 1;
            if n == 0 {
                bail!("empty transport frame");
            }
            out.extend_from_slice(&plain[1..n]);
            if out.len() > self.max_message {
                bail!("message exceeds {} bytes", self.max_message);
            }
            if plain[0] == 1 {
                return Ok(out);
            }
        }
    }
}

fn noise_builder<'a>(identity: &'a Identity, prologue: &'a [u8]) -> Result<snow::Builder<'a>> {
    Ok(snow::Builder::new(NOISE_XX.parse()?)
        .local_private_key(&identity.private_key)?
        .prologue(prologue)?)
}

/// Dials a member for a sync session.
pub async fn connect_sync(
    addr: SocketAddr,
    identity: &Identity,
    group_id: GroupId,
) -> Result<Session> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut stream = TcpStream::connect(addr).await?;
        let preamble = postcard::to_stdvec(&Preamble::Sync {
            proto: PROTOCOL_VERSION,
            group_id,
        })?;
        write_frame(&mut stream, &preamble).await?;
        let prologue = prologue(&preamble);
        let hs = noise_builder(identity, &prologue)?.build_initiator()?;
        let transport = run_handshake(&mut stream, hs).await?;
        Session::new(stream, transport)
    })
    .await
    .map_err(|_| anyhow!("handshake with {addr} timed out"))?
}

/// An accepted TCP connection whose preamble has been read.
pub struct Incoming {
    pub stream: TcpStream,
    pub preamble: Preamble,
    pub(crate) preamble_bytes: Vec<u8>,
}

impl Incoming {
    pub async fn read(mut stream: TcpStream) -> Result<Self> {
        let preamble_bytes = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream))
            .await
            .map_err(|_| anyhow!("timed out waiting for preamble"))??;
        let preamble = postcard::from_bytes(&preamble_bytes).context("bad preamble")?;
        Ok(Self {
            stream,
            preamble,
            preamble_bytes,
        })
    }

    /// Completes the responder side of a sync handshake.
    pub async fn accept_sync(mut self, identity: &Identity) -> Result<Session> {
        tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let prologue = prologue(&self.preamble_bytes);
            let hs = noise_builder(identity, &prologue)?.build_responder()?;
            let transport = run_handshake(&mut self.stream, hs).await?;
            Session::new(self.stream, transport)
        })
        .await
        .map_err(|_| anyhow!("handshake timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn handshake_and_large_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let group = GroupId([7; 16]);
        let b2 = b.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let inc = Incoming::read(stream).await.unwrap();
            assert_eq!(
                inc.preamble,
                Preamble::Sync {
                    proto: PROTOCOL_VERSION,
                    group_id: group
                }
            );
            let mut s = inc.accept_sync(&b2).await.unwrap();
            let big: Vec<u8> = s.recv().await.unwrap();
            s.send(&big.len()).await.unwrap();
            s.remote_public_key
        });
        let mut s = connect_sync(addr, &a, group).await.unwrap();
        assert_eq!(s.remote_public_key, b.public_key);
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        s.send(&big).await.unwrap();
        let len: usize = s.recv().await.unwrap();
        assert_eq!(len, 200_000);
        assert_eq!(server.await.unwrap(), a.public_key);
    }
}
