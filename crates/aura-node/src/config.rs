use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 47820;
/// HTTPS port of the phone page served by the fofoqueiro.
pub const DEFAULT_WEB_PORT: u16 = 47821;

/// User-editable settings, stored as `config.toml` in the data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name shown to other devices. Defaults to the hostname.
    pub device_name: String,
    /// TCP port to listen on (0 = any free port).
    pub port: u16,
    /// Advertise and browse with mDNS.
    pub discovery: bool,
    /// Extra peers to dial, for networks where multicast does not work.
    pub static_peers: Vec<SocketAddr>,
    pub poll_interval_ms: u64,
    /// Largest entry that is synced (images are PNG-compressed first).
    pub max_entry_bytes: usize,
    pub history_entries: usize,
    pub history_bytes: usize,
    /// "keyring" (OS keychain) or "file" (0600 file in the data dir).
    pub secret_store: SecretStoreKind,
    /// Take part in the election of the PC that serves the phone page.
    pub web_enabled: bool,
    pub web_port: u16,
    /// Version of this file's defaults, for one-time migrations.
    pub config_version: u32,
}

const CONFIG_VERSION: u32 = 2;
/// The history length before search existed; raised to [`Config::default`]'s.
const OLD_DEFAULT_HISTORY: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStoreKind {
    Keyring,
    File,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            port: DEFAULT_PORT,
            discovery: true,
            static_peers: Vec::new(),
            poll_interval_ms: 250,
            max_entry_bytes: 32 * 1024 * 1024,
            history_entries: 500,
            history_bytes: 100 * 1024 * 1024,
            // Debug builds get a new code signature on every build, which makes macOS
            // ask for keychain access each time; use a file while developing. Without a
            // usable keyring (e.g. no Secret Service on Linux) the store falls back to the file.
            secret_store: if cfg!(debug_assertions) {
                SecretStoreKind::File
            } else {
                SecretStoreKind::Keyring
            },
            web_enabled: true,
            web_port: DEFAULT_WEB_PORT,
            config_version: CONFIG_VERSION,
        }
    }
}

impl Config {
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.max(20))
    }

    /// Loads `config.toml` from `dir`, writing the defaults if it does not exist yet.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("config.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load_or_init(dir: &Path) -> Result<Self> {
        let path = dir.join("config.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let mut cfg: Self =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            let table: toml::Table = toml::from_str(&text)?;
            if !table.contains_key("config_version") {
                // Written before search existed: a still-default history length of 50 is
                // raised so search reaches further back. A value the user set is kept.
                if cfg.history_entries == OLD_DEFAULT_HISTORY {
                    cfg.history_entries = Self::default().history_entries;
                }
                cfg.config_version = CONFIG_VERSION;
                cfg.save(dir)?;
            }
            return Ok(cfg);
        }
        let cfg = Self::default();
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }
}

/// Platform data directory, e.g. `~/Library/Application Support/dev.aura.aura-clipboard`.
pub fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "aura", "aura-clipboard")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".aura-clipboard"))
}

fn default_device_name() -> String {
    std::env::var("AURA_DEVICE_NAME")
        .ok()
        .or_else(hostname)
        .unwrap_or_else(|| "aura-device".to_string())
}

fn hostname() -> Option<String> {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("hostname").output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        let name = name.strip_suffix(".local").unwrap_or(&name).to_string();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_from(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        let cfg = Config::load_or_init(dir.path()).unwrap();
        // The migration is saved, so loading again gives the same result.
        let again = Config::load_or_init(dir.path()).unwrap();
        assert_eq!(again.history_entries, cfg.history_entries);
        cfg
    }

    #[test]
    fn old_default_history_is_raised_but_custom_values_are_kept() {
        assert_eq!(load_from("history_entries = 50\n").history_entries, 500);
        assert_eq!(load_from("history_entries = 120\n").history_entries, 120);
        assert_eq!(
            load_from("history_entries = 50\nconfig_version = 2\n").history_entries,
            50,
            "a 50 chosen after the migration stays"
        );
    }
}
