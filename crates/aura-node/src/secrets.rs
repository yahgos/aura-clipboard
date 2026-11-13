//! Device secrets: the Noise private key and the group's password key `w`.
//! Kept together in one blob so the OS keychain prompts at most once.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aura_net::Identity;
use aura_net::proto::WebCert;
use serde::{Deserialize, Serialize};

use crate::config::SecretStoreKind;

#[derive(Clone, Serialize, Deserialize)]
pub struct Secrets {
    pub identity: Identity,
    pub password_key: Option<[u8; 32]>,
    pub web_cert: Option<WebCert>,
}

/// Layout written before the phone page existed; still readable.
#[derive(Deserialize)]
struct SecretsV1 {
    identity: Identity,
    password_key: Option<[u8; 32]>,
}

fn decode(bytes: &[u8]) -> Result<Secrets> {
    if let Ok(s) = postcard::from_bytes::<Secrets>(bytes) {
        return Ok(s);
    }
    let v1: SecretsV1 = postcard::from_bytes(bytes).context("corrupt secrets")?;
    Ok(Secrets {
        identity: v1.identity,
        password_key: v1.password_key,
        web_cert: None,
    })
}

pub struct SecretStore {
    kind: SecretStoreKind,
    file: PathBuf,
    /// Keychain account name; includes the data dir so several dev instances don't collide.
    account: String,
    /// Set when reading the keyring failed: from then on this run only uses the file, so a
    /// transient keyring error can never overwrite the real secrets in the keyring.
    keyring_failed: Cell<bool>,
    /// Makes every keyring call fail, to test the fallbacks.
    #[cfg(test)]
    broken_keyring: bool,
}

const SERVICE: &str = "dev.aura.aura-clipboard";

impl SecretStore {
    pub fn new(kind: SecretStoreKind, data_dir: &Path) -> Self {
        let account = format!(
            "secrets:{}",
            blake3::hash(data_dir.to_string_lossy().as_bytes()).to_hex()
        );
        Self {
            kind,
            file: data_dir.join("secrets.bin"),
            account: account[..24].to_string(),
            keyring_failed: Cell::new(false),
            #[cfg(test)]
            broken_keyring: false,
        }
    }

    /// Loads the secrets, creating a fresh identity on first run.
    ///
    /// `in_group` says whether the database records a group. Then the keyring holds
    /// that group's key, and if the keyring can't be read (locked right after login,
    /// an unanswered prompt after an OS update) this fails instead of starting over
    /// with a new identity: that would drop the group.
    pub fn load_or_create(&self, in_group: bool) -> Result<Secrets> {
        match self.read() {
            Ok(Some(bytes)) => {
                let secrets = decode(&bytes)?;
                if in_group && secrets.password_key.is_none() && self.keyring_failed.get() {
                    // A fallback file left by an earlier run that hit the same problem.
                    bail!(
                        "the system keychain can't be read, and {} has no group key",
                        self.file.display()
                    );
                }
                Ok(secrets)
            }
            Ok(None) => self.create(),
            Err(e) if in_group => {
                Err(e.context("can't read the stored keys of the group this PC is in"))
            }
            Err(e) => {
                // Not in a group, so there is nothing to lose: start with the file.
                tracing::warn!(
                    "OS keyring unavailable ({e:#}); storing secrets in {}",
                    self.file.display()
                );
                self.keyring_failed.set(true);
                self.create()
            }
        }
    }

    fn create(&self) -> Result<Secrets> {
        let secrets = Secrets {
            identity: Identity::generate()?,
            password_key: None,
            web_cert: None,
        };
        self.save(&secrets)?;
        Ok(secrets)
    }

    pub fn save(&self, secrets: &Secrets) -> Result<()> {
        let bytes = postcard::to_stdvec(secrets)?;
        match self.kind {
            SecretStoreKind::File => write_private(&self.file, &bytes),
            SecretStoreKind::Keyring if self.keyring_failed.get() => {
                write_private(&self.file, &bytes)
            }
            SecretStoreKind::Keyring => match self.keyring_set(&bytes) {
                Ok(()) => {
                    // Secrets may have lived in the fallback file before; keep one copy only.
                    let _ = std::fs::remove_file(&self.file);
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        "OS keyring unavailable ({e:#}); storing secrets in {}",
                        self.file.display()
                    );
                    write_private(&self.file, &bytes)
                }
            },
        }
    }

    fn read(&self) -> Result<Option<Vec<u8>>> {
        match self.kind {
            SecretStoreKind::File => self.read_file(),
            SecretStoreKind::Keyring => match self.keyring_get() {
                Ok(Some(b)) => Ok(Some(b)),
                // Nothing in the keyring: secrets may be in the fallback file (written while
                // the keyring was unavailable, or before switching stores). The next save
                // moves them into the keyring.
                Ok(None) => self.read_file(),
                // Only a fallback file can stand in for the keyring; without one the
                // caller decides (see `load_or_create`).
                Err(e) => match self.read_file()? {
                    Some(bytes) => {
                        tracing::warn!(
                            "OS keyring unavailable ({e:#}); using {}",
                            self.file.display()
                        );
                        self.keyring_failed.set(true);
                        Ok(Some(bytes))
                    }
                    None => Err(e),
                },
            },
        }
    }

    fn read_file(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(&self.file) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// macOS Keychain, Windows Credential Manager, or the Secret Service (GNOME
    /// Keyring / KWallet) over D-Bus on Linux.
    fn keyring_get(&self) -> Result<Option<Vec<u8>>> {
        #[cfg(test)]
        if self.broken_keyring {
            bail!("keychain locked");
        }
        let entry = keyring_entry(&self.account)?;
        match entry.get_secret() {
            Ok(b) => Ok(Some(b)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn keyring_set(&self, bytes: &[u8]) -> Result<()> {
        #[cfg(test)]
        if self.broken_keyring {
            bail!("keychain locked");
        }
        keyring_entry(&self.account)?.set_secret(bytes)?;
        Ok(())
    }
}

fn keyring_entry(account: &str) -> Result<keyring::Entry> {
    match keyring::Entry::new(SERVICE, account) {
        // Report why the platform store failed to start (e.g. no D-Bus session on Linux).
        Err(keyring::Error::NoDefaultStore) => match keyring::Entry::store_status() {
            Err(cause) => Err(anyhow::anyhow!("no keyring: {cause}")),
            Ok(()) => Err(keyring::Error::NoDefaultStore.into()),
        },
        other => Ok(other?),
    }
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_roundtrip_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(SecretStoreKind::File, dir.path());
        let first = store.load_or_create(false).unwrap();
        let again = store.load_or_create(false).unwrap();
        assert_eq!(first.identity.public_key, again.identity.public_key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets.bin"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    fn broken_keyring(dir: &Path) -> SecretStore {
        SecretStore {
            broken_keyring: true,
            ..SecretStore::new(SecretStoreKind::Keyring, dir)
        }
    }

    #[test]
    fn unreadable_keyring_keeps_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let store = broken_keyring(dir.path());
        assert!(store.load_or_create(true).is_err());
        // Nothing was written that a later run could mistake for our secrets.
        assert!(!dir.path().join("secrets.bin").exists());
    }

    #[test]
    fn unreadable_keyring_outside_a_group_falls_back_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = broken_keyring(dir.path());
        let first = store.load_or_create(false).unwrap();
        assert!(dir.path().join("secrets.bin").exists());
        let again = broken_keyring(dir.path()).load_or_create(false).unwrap();
        assert_eq!(first.identity.public_key, again.identity.public_key);
    }

    #[test]
    fn unreadable_keyring_uses_a_fallback_file_with_the_group_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = SecretStore::new(SecretStoreKind::File, dir.path());
        let mut secrets = file.load_or_create(false).unwrap();
        secrets.password_key = Some([7; 32]);
        file.save(&secrets).unwrap();

        let loaded = broken_keyring(dir.path()).load_or_create(true).unwrap();
        assert_eq!(loaded.password_key, Some([7; 32]));
    }

    #[test]
    fn unreadable_keyring_rejects_a_fallback_file_without_the_group_key() {
        let dir = tempfile::tempdir().unwrap();
        SecretStore::new(SecretStoreKind::File, dir.path())
            .load_or_create(false)
            .unwrap();
        assert!(broken_keyring(dir.path()).load_or_create(true).is_err());
    }
}
