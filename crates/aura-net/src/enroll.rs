//! Joining a group with its passphrase.
//!
//! ```text
//! joiner J                                   member M
//!   ── Preamble::Join ───────────────────────▶
//!   ◀────────────── Challenge{group_id, name, spake_M}
//!   w = Argon2id(passphrase, salt = group_id)
//!   ── JoinSpake{spake_J} ───────────────────▶
//!   K = SPAKE2(w) on both sides; psk = KDF(K)
//!   ══ Noise XXpsk3(psk) handshake ══════════   (fails on a wrong passphrase)
//!   ── JoinInfo{name, port} (encrypted) ─────▶
//!   ◀──────────── JoinResult::Welcome{membership, w, …}
//! ```
//!
//! The passphrase never crosses the wire, and an eavesdropper cannot run an
//! offline dictionary attack; only online guesses are possible, and members
//! rate-limit those ([`RateLimiter`]).

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use aura_core::{GroupId, PROTOCOL_VERSION};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::net::TcpStream;

use crate::Identity;
use crate::identity::NOISE_XX_PSK3;
use crate::proto::{JoinChallenge, JoinInfo, JoinResult, JoinSpake, Preamble, Welcome};
use crate::session::{
    HANDSHAKE_TIMEOUT, Incoming, Session, prologue, read_plain, run_handshake, write_frame,
    write_plain,
};

const PSK_CONTEXT: &str = "aura-clipboard v1 enrollment psk";

/// `w = Argon2id(passphrase, salt = group_id)`. Deliberately slow; call from a blocking thread.
pub fn password_key(passphrase: &str, group_id: GroupId) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(passphrase.as_bytes(), &group_id.0, &mut out)
        .map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(out)
}

fn spake_start(w: &[u8; 32], group_id: GroupId) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let id = [b"aura-join:".as_slice(), &group_id.0].concat();
    Spake2::<Ed25519Group>::start_symmetric(&Password::new(w), &SpakeIdentity::new(&id))
}

fn psk_from(key: &[u8]) -> [u8; 32] {
    blake3::derive_key(PSK_CONTEXT, key)
}

fn psk_builder<'a>(
    identity: &'a Identity,
    prologue: &'a [u8],
    psk: &'a [u8; 32],
) -> Result<snow::Builder<'a>> {
    Ok(snow::Builder::new(NOISE_XX_PSK3.parse()?)
        .local_private_key(&identity.private_key)?
        .prologue(prologue)?
        .psk(3, psk)?)
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("join refused: {0}")]
    Refused(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Joiner side: contacts a member at `addr` and returns the group credentials plus
/// the admitting member's static public key.
pub async fn join(
    addr: SocketAddr,
    identity: &Identity,
    passphrase: &str,
    info: JoinInfo,
) -> Result<(Welcome, [u8; 32]), JoinError> {
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connecting to {addr} timed out"))?
        .with_context(|| format!("connecting to {addr}"))?;
    let preamble = postcard::to_stdvec(&Preamble::Join {
        proto: PROTOCOL_VERSION,
    })
    .map_err(anyhow::Error::from)?;
    write_frame(&mut stream, &preamble).await?;

    let challenge: JoinChallenge = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_plain(&mut stream))
        .await
        .map_err(|_| anyhow!("timed out"))??;
    let (group_id, server_spake) = match challenge {
        JoinChallenge::Challenge {
            group_id, spake, ..
        } => (group_id, spake),
        JoinChallenge::Refused { reason } => return Err(JoinError::Refused(reason)),
    };

    let pass = passphrase.to_owned();
    let w = tokio::task::spawn_blocking(move || password_key(&pass, group_id))
        .await
        .map_err(anyhow::Error::from)??;
    let (spake, msg) = spake_start(&w, group_id);
    write_plain(&mut stream, &JoinSpake { spake: msg }).await?;
    let key = spake
        .finish(&server_spake)
        .map_err(|e| anyhow!("spake2: {e:?}"))?;
    let psk = psk_from(&key);

    let prologue = prologue(&preamble);
    let hs = psk_builder(identity, &prologue, &psk)
        .map_err(JoinError::Other)?
        .build_initiator()
        .map_err(anyhow::Error::from)?;
    // With a wrong passphrase the member cannot decrypt our final handshake message and hangs up.
    let transport = tokio::time::timeout(HANDSHAKE_TIMEOUT, run_handshake(&mut stream, hs))
        .await
        .map_err(|_| anyhow!("handshake timed out"))?
        .map_err(|_| JoinError::WrongPassphrase)?;
    let mut session = Session::new(stream, transport)?;
    session.send(&info).await?;
    match session.recv::<JoinResult>().await {
        Ok(JoinResult::Welcome(w)) => Ok((w, session.remote_public_key)),
        Ok(JoinResult::Refused(reason)) => Err(JoinError::Refused(reason)),
        // Connection closed right after the handshake: the member could not verify us.
        Err(_) => Err(JoinError::WrongPassphrase),
    }
}

/// What a member needs to admit new devices.
#[derive(Clone)]
pub struct JoinAuthority {
    pub group_id: GroupId,
    pub group_name: String,
    pub password_key: [u8; 32],
}

/// Member side of [`join`]. `admit` is called once the joiner proved knowledge of the
/// passphrase; it records the new member and returns the welcome package.
pub async fn serve_join<F, Fut>(
    mut incoming: Incoming,
    identity: &Identity,
    authority: Option<JoinAuthority>,
    admit: F,
) -> Result<(), JoinError>
where
    F: FnOnce([u8; 32], JoinInfo) -> Fut,
    Fut: Future<Output = Result<Welcome>>,
{
    let stream = &mut incoming.stream;
    let Some(auth) = authority else {
        write_plain(
            stream,
            &JoinChallenge::Refused {
                reason: "this device is not in a group".into(),
            },
        )
        .await?;
        return Err(JoinError::Refused("not in a group".into()));
    };
    let (spake, msg) = spake_start(&auth.password_key, auth.group_id);
    write_plain(
        stream,
        &JoinChallenge::Challenge {
            group_id: auth.group_id,
            group_name: auth.group_name.clone(),
            spake: msg,
        },
    )
    .await?;
    // Argon2 on the joiner can take a moment.
    let joiner: JoinSpake = tokio::time::timeout(HANDSHAKE_TIMEOUT * 3, read_plain(stream))
        .await
        .map_err(|_| anyhow!("timed out"))??;
    let key = spake
        .finish(&joiner.spake)
        .map_err(|e| anyhow!("spake2: {e:?}"))?;
    let psk = psk_from(&key);

    let prologue = prologue(&incoming.preamble_bytes);
    let hs = psk_builder(identity, &prologue, &psk)?
        .build_responder()
        .map_err(anyhow::Error::from)?;
    let transport =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, run_handshake(&mut incoming.stream, hs))
            .await
            .map_err(|_| anyhow!("handshake timed out"))?
            .map_err(|_| JoinError::WrongPassphrase)?;
    let mut session = Session::new(incoming.stream, transport)?;
    let info: JoinInfo = session.recv().await?;
    let result = match admit(session.remote_public_key, info).await {
        Ok(welcome) => JoinResult::Welcome(welcome),
        Err(e) => JoinResult::Refused(e.to_string()),
    };
    session.send(&result).await?;
    session.writer.shutdown().await;
    Ok(())
}

/// Throttles join attempts: at most `per_minute` failures per IP, plus a global
/// exponential backoff after repeated failures from anywhere.
pub struct RateLimiter {
    per_minute: usize,
    failures: HashMap<IpAddr, VecDeque<Instant>>,
    global_failures: u32,
    blocked_until: Option<Instant>,
}

impl RateLimiter {
    pub fn new(per_minute: usize) -> Self {
        Self {
            per_minute,
            failures: HashMap::new(),
            global_failures: 0,
            blocked_until: None,
        }
    }

    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.blocked_until.is_some_and(|t| now < t) {
            return false;
        }
        let q = self.failures.entry(ip).or_default();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            q.pop_front();
        }
        q.len() < self.per_minute
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        self.failures.entry(ip).or_default().push_back(now);
        self.global_failures += 1;
        if self.global_failures >= 10 {
            let secs = 2u64.saturating_pow((self.global_failures - 10).min(8));
            self.blocked_until = Some(now + Duration::from_secs(secs));
        }
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
        self.global_failures = 0;
        self.blocked_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::Membership;
    use tokio::net::TcpListener;

    async fn run(
        passphrase_member: &'static str,
        passphrase_joiner: &'static str,
    ) -> Result<Welcome, JoinError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let member = Identity::generate().unwrap();
        let member_pk = member.public_key;
        let joiner = Identity::generate().unwrap();
        let group_id = GroupId([3; 16]);
        let w = password_key(passphrase_member, group_id).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let inc = Incoming::read(stream).await.unwrap();
            let auth = JoinAuthority {
                group_id,
                group_name: "Home".into(),
                password_key: w,
            };
            serve_join(inc, &member, Some(auth), async |pk, info| {
                assert_eq!(info.name, "laptop");
                assert_eq!(aura_core::DeviceId::from_public_key(&pk), joiner_id(&pk));
                Ok(Welcome {
                    group_id,
                    group_name: "Home".into(),
                    key: crate::proto::GroupKey {
                        version: Default::default(),
                        password_key: w,
                    },
                    membership: Membership::new(),
                    addr_hints: vec![],
                })
            })
            .await
        });
        let res = join(
            addr,
            &joiner,
            passphrase_joiner,
            JoinInfo {
                name: "laptop".into(),
                listen_port: 1,
            },
        )
        .await
        .map(|(w, pk)| {
            assert_eq!(pk, member_pk);
            w
        });
        let server_res = server.await.unwrap();
        if res.is_ok() {
            assert!(server_res.is_ok());
        } else {
            assert!(matches!(server_res, Err(JoinError::WrongPassphrase)));
        }
        res
    }

    fn joiner_id(pk: &[u8; 32]) -> aura_core::DeviceId {
        aura_core::DeviceId::from_public_key(pk)
    }

    #[tokio::test]
    async fn correct_passphrase_joins() {
        let w = run("correct horse", "correct horse").await.unwrap();
        assert_eq!(w.group_name, "Home");
        assert_eq!(
            w.key.password_key,
            password_key("correct horse", GroupId([3; 16])).unwrap()
        );
    }

    #[tokio::test]
    async fn wrong_passphrase_is_rejected() {
        assert!(matches!(
            run("correct horse", "battery staple").await,
            Err(JoinError::WrongPassphrase)
        ));
    }

    #[test]
    fn rate_limiter_blocks_after_failures() {
        let mut rl = RateLimiter::new(3);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        let now = Instant::now();
        for _ in 0..3 {
            assert!(rl.allow(ip, now));
            rl.record_failure(ip, now);
        }
        assert!(!rl.allow(ip, now));
        assert!(rl.allow(ip, now + Duration::from_secs(61)));
        assert!(rl.allow("10.0.0.3".parse().unwrap(), now));
    }
}
