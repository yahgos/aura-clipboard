//! Group membership as a 2P-set CRDT: a grow-only set of added members plus a
//! grow-only set of removed device ids. A removed device can never come back
//! with the same key; it has to re-enroll with a fresh identity.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::DeviceId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub device_id: DeviceId,
    /// Noise X25519 static public key.
    pub public_key: [u8; 32],
    pub name: String,
    pub added_by: DeviceId,
    pub added_at_ms: u64,
}

impl Member {
    /// Deterministic winner when two replicas hold different metadata for the same device
    /// (e.g. after a rename): the most recently added record wins.
    fn merge_key(&self) -> (u64, &str, DeviceId) {
        (self.added_at_ms, self.name.as_str(), self.added_by)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    added: BTreeMap<DeviceId, Member>,
    removed: BTreeSet<DeviceId>,
}

impl Membership {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or refreshes) a member. Returns false if the device was removed before.
    pub fn add(&mut self, member: Member) -> bool {
        if self.removed.contains(&member.device_id) {
            return false;
        }
        match self.added.get(&member.device_id) {
            Some(existing) if existing.merge_key() >= member.merge_key() => {}
            _ => {
                self.added.insert(member.device_id, member);
            }
        }
        true
    }

    pub fn remove(&mut self, id: DeviceId) {
        self.removed.insert(id);
    }

    pub fn is_member(&self, id: &DeviceId) -> bool {
        self.added.contains_key(id) && !self.removed.contains(id)
    }

    pub fn was_removed(&self, id: &DeviceId) -> bool {
        self.removed.contains(id)
    }

    /// True if the device was ever admitted (even if removed since). Used to accept
    /// history entries that originated on a device that has since left.
    pub fn ever_added(&self, id: &DeviceId) -> bool {
        self.added.contains_key(id)
    }

    pub fn get(&self, id: &DeviceId) -> Option<&Member> {
        self.added.get(id).filter(|_| !self.removed.contains(id))
    }

    /// Member record even if the device has been removed since (for display names).
    pub fn get_any(&self, id: &DeviceId) -> Option<&Member> {
        self.added.get(id)
    }

    /// Live members, ordered by device id.
    pub fn live(&self) -> impl Iterator<Item = &Member> {
        self.added
            .values()
            .filter(|m| !self.removed.contains(&m.device_id))
    }

    pub fn live_count(&self) -> usize {
        self.live().count()
    }

    /// Merges another replica in. Returns true if anything changed locally.
    pub fn merge(&mut self, other: &Membership) -> bool {
        let before = self.clone();
        self.removed.extend(other.removed.iter().copied());
        for m in other.added.values() {
            match self.added.get(&m.device_id) {
                Some(existing) if existing.merge_key() >= m.merge_key() => {}
                _ => {
                    self.added.insert(m.device_id, m.clone());
                }
            }
        }
        *self != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn member(n: u8, at: u64, name: &str) -> Member {
        Member {
            device_id: DeviceId([n; 16]),
            public_key: [n; 32],
            name: name.into(),
            added_by: DeviceId([0; 16]),
            added_at_ms: at,
        }
    }

    #[test]
    fn removal_wins_and_is_permanent() {
        let mut a = Membership::new();
        a.add(member(1, 1, "a"));
        let mut b = a.clone();
        b.remove(DeviceId([1; 16]));
        a.add(member(1, 2, "renamed"));
        assert!(a.merge(&b));
        assert!(!a.is_member(&DeviceId([1; 16])));
        assert!(!a.add(member(1, 3, "again")));
        assert!(a.ever_added(&DeviceId([1; 16])));
    }

    #[derive(Debug, Clone)]
    enum Op {
        Add(u8, u64, u8),
        Remove(u8),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..6, 0u64..4, 0u8..3).prop_map(|(d, t, n)| Op::Add(d, t, n)),
            (0u8..6).prop_map(Op::Remove),
        ]
    }

    fn build(ops: &[Op]) -> Membership {
        let mut m = Membership::new();
        for o in ops {
            match o {
                Op::Add(d, t, n) => {
                    m.add(member(*d, *t, &format!("n{n}")));
                }
                Op::Remove(d) => m.remove(DeviceId([*d; 16])),
            }
        }
        m
    }

    fn merged(a: &Membership, b: &Membership) -> Membership {
        let mut x = a.clone();
        x.merge(b);
        x
    }

    proptest! {
        #[test]
        fn merge_is_a_semilattice(
            a in prop::collection::vec(op(), 0..12),
            b in prop::collection::vec(op(), 0..12),
            c in prop::collection::vec(op(), 0..12),
        ) {
            let (a, b, c) = (build(&a), build(&b), build(&c));
            // commutative
            prop_assert_eq!(merged(&a, &b), merged(&b, &a));
            // associative
            prop_assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
            // idempotent
            prop_assert_eq!(merged(&a, &a), a.clone());
        }
    }
}
