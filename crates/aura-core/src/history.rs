//! Bounded clipboard history with dedup. The greatest entry id is the shared
//! "current" clipboard; delivery order does not matter.

use std::collections::{BTreeMap, BTreeSet};

use crate::{Entry, EntryId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// First time we see this entry. `is_current` is true if it is now the newest one.
    New { is_current: bool },
    /// Already seen (or older than everything we still keep); ignore it.
    Duplicate,
}

#[derive(Debug, Clone)]
pub struct History {
    entries: BTreeMap<EntryId, Entry>,
    /// Ids seen recently, including evicted ones, so re-flooded entries are ignored.
    seen: BTreeSet<EntryId>,
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
}

const SEEN_CAP: usize = 4096;

impl History {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            seen: BTreeSet::new(),
            max_entries: max_entries.max(1),
            max_bytes,
            bytes: 0,
        }
    }

    pub fn insert(&mut self, entry: Entry) -> InsertOutcome {
        let id = entry.id;
        if self.seen.contains(&id) || self.entries.contains_key(&id) {
            return InsertOutcome::Duplicate;
        }
        // Older than everything we keep in a full history: it would be evicted right away.
        if self.entries.len() >= self.max_entries
            && self
                .entries
                .keys()
                .next()
                .is_some_and(|oldest| id < *oldest)
        {
            self.mark_seen(id);
            return InsertOutcome::Duplicate;
        }
        let is_current = self.current_id().is_none_or(|cur| id > cur);
        self.bytes += entry.content.size();
        self.entries.insert(id, entry);
        self.mark_seen(id);
        self.evict();
        InsertOutcome::New { is_current }
    }

    fn mark_seen(&mut self, id: EntryId) {
        self.seen.insert(id);
        while self.seen.len() > SEEN_CAP {
            self.seen.pop_first();
        }
    }

    fn evict(&mut self) {
        // Always keep at least the current entry, whatever its size.
        while self.entries.len() > 1
            && (self.entries.len() > self.max_entries || self.bytes > self.max_bytes)
        {
            if let Some((_, e)) = self.entries.pop_first() {
                self.bytes -= e.content.size();
            }
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.values().next_back()
    }

    pub fn current_id(&self) -> Option<EntryId> {
        self.entries.keys().next_back().copied()
    }

    pub fn get(&self, id: &EntryId) -> Option<&Entry> {
        self.entries.get(id)
    }

    /// Newest first.
    pub fn iter_newest(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().rev()
    }

    pub fn ids(&self) -> impl Iterator<Item = &EntryId> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.seen.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Content, DeviceId, Hlc};
    use proptest::prelude::*;

    fn entry(wall: u64, dev: u8, text: &str) -> Entry {
        Entry {
            id: EntryId {
                hlc: Hlc {
                    wall_ms: wall,
                    counter: 0,
                },
                origin: DeviceId([dev; 16]),
            },
            content: Content::Text(text.into()),
        }
    }

    #[test]
    fn dedup_and_current() {
        let mut h = History::new(10, 1 << 20);
        assert_eq!(
            h.insert(entry(2, 1, "b")),
            InsertOutcome::New { is_current: true }
        );
        assert_eq!(
            h.insert(entry(1, 1, "a")),
            InsertOutcome::New { is_current: false }
        );
        assert_eq!(h.insert(entry(2, 1, "b")), InsertOutcome::Duplicate);
        assert_eq!(h.current().unwrap().content, Content::Text("b".into()));
    }

    #[test]
    fn evicts_oldest_by_count_and_bytes() {
        let mut h = History::new(2, 1 << 20);
        h.insert(entry(1, 1, "a"));
        h.insert(entry(2, 1, "b"));
        h.insert(entry(3, 1, "c"));
        assert_eq!(h.len(), 2);
        // Evicted entry is not re-accepted.
        assert_eq!(h.insert(entry(1, 1, "a")), InsertOutcome::Duplicate);

        let mut h = History::new(10, 5);
        h.insert(entry(1, 1, "aaaa"));
        h.insert(entry(2, 1, "bbbb"));
        assert_eq!(h.len(), 1);
        assert_eq!(h.current_id().unwrap().hlc.wall_ms, 2);
    }

    proptest! {
        #[test]
        fn any_delivery_order_converges(
            mut items in prop::collection::vec((0u64..50, 0u8..4), 1..40),
            seed in any::<u64>(),
        ) {
            let entries: Vec<Entry> =
                items.drain(..).map(|(w, d)| entry(w, d, &format!("{w}-{d}"))).collect();
            let mut shuffled = entries.clone();
            // cheap deterministic shuffle
            let mut s = seed;
            for i in (1..shuffled.len()).rev() {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                shuffled.swap(i, (s >> 33) as usize % (i + 1));
            }
            let mut a = History::new(8, 1 << 20);
            let mut b = History::new(8, 1 << 20);
            for e in entries { a.insert(e); }
            for e in shuffled.iter().cloned() { b.insert(e.clone()); b.insert(e); }
            prop_assert_eq!(a.current_id(), b.current_id());
            prop_assert_eq!(a.ids().collect::<Vec<_>>(), b.ids().collect::<Vec<_>>());
        }
    }
}
