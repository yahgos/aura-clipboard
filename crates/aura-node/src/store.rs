//! Persistent state in a redb database: group info, membership and history.
//! Opening the database also takes an exclusive lock, which keeps a second
//! instance from running on the same data directory.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use aura_core::{DeviceId, Entry, EntryId, GroupId, Membership};
use aura_net::proto::KeyVersion;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const HISTORY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("history");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub id: GroupId,
    pub name: String,
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("state.redb");
        let db = Database::create(&path).with_context(|| {
            format!(
                "opening {} (is another Aura instance using this data dir?)",
                path.display()
            )
        })?;
        let tx = db.begin_write()?;
        tx.open_table(META)?;
        tx.open_table(HISTORY)?;
        tx.commit()?;
        Ok(Self { db })
    }

    fn get_meta<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(META)?;
        match t.get(key)? {
            Some(v) => Ok(Some(postcard::from_bytes(v.value())?)),
            None => Ok(None),
        }
    }

    fn put_meta<T: Serialize>(&self, key: &str, value: Option<&T>) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(META)?;
            match value {
                Some(v) => {
                    t.insert(key, postcard::to_stdvec(v)?.as_slice())?;
                }
                None => {
                    t.remove(key)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn group(&self) -> Result<Option<GroupInfo>> {
        self.get_meta("group")
    }

    pub fn set_group(&self, group: Option<&GroupInfo>) -> Result<()> {
        self.put_meta("group", group)
    }

    pub fn membership(&self) -> Result<Membership> {
        Ok(self.get_meta("membership")?.unwrap_or_default())
    }

    pub fn set_membership(&self, m: &Membership) -> Result<()> {
        self.put_meta("membership", Some(m))
    }

    pub fn addr_book(&self) -> Result<Vec<(DeviceId, BTreeSet<SocketAddr>)>> {
        Ok(self.get_meta("addrs")?.unwrap_or_default())
    }

    pub fn set_addr_book(&self, book: &[(DeviceId, BTreeSet<SocketAddr>)]) -> Result<()> {
        self.put_meta("addrs", Some(&book))
    }

    pub fn key_version(&self) -> Result<KeyVersion> {
        Ok(self.get_meta("key_version")?.unwrap_or_default())
    }

    pub fn set_key_version(&self, v: KeyVersion) -> Result<()> {
        self.put_meta("key_version", Some(&v))
    }

    pub fn paused(&self) -> Result<bool> {
        Ok(self.get_meta("paused")?.unwrap_or(false))
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.put_meta("paused", Some(&paused))
    }

    pub fn load_history(&self) -> Result<Vec<Entry>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HISTORY)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            match postcard::from_bytes(v.value()) {
                Ok(e) => out.push(e),
                Err(e) => tracing::warn!("skipping unreadable history entry: {e}"),
            }
        }
        Ok(out)
    }

    /// Stores `entry` and deletes every stored entry not in `keep`.
    pub fn put_entry(&self, entry: &Entry, keep: &BTreeSet<EntryId>) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(HISTORY)?;
            if keep.contains(&entry.id) {
                t.insert(
                    entry.id.to_key().as_slice(),
                    postcard::to_stdvec(entry)?.as_slice(),
                )?;
            }
            let stale: Vec<Vec<u8>> = t
                .iter()?
                .filter_map(|r| r.ok())
                .map(|(k, _)| k.value().to_vec())
                .filter(|k| EntryId::from_key(k).is_none_or(|id| !keep.contains(&id)))
                .collect();
            for k in stale {
                t.remove(k.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.delete_table(HISTORY)?;
        tx.open_table(HISTORY)?;
        tx.commit()?;
        Ok(())
    }
}
