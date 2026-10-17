//! Hybrid logical clock (Kulkarni et al.): wall-clock milliseconds plus a
//! logical counter. Timestamps are monotonic per device and never go behind
//! anything the device has observed, so last-writer-wins stays sane even when
//! PC clocks drift.

use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u32,
}

#[derive(Clone, Debug, Default)]
pub struct HlcClock {
    last: Hlc,
}

impl HlcClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Timestamp for a local event, given the current physical time.
    pub fn now(&mut self, physical_ms: u64) -> Hlc {
        self.last = if physical_ms > self.last.wall_ms {
            Hlc {
                wall_ms: physical_ms,
                counter: 0,
            }
        } else {
            Hlc {
                wall_ms: self.last.wall_ms,
                counter: self.last.counter + 1,
            }
        };
        self.last
    }

    /// Merge a timestamp received from another device.
    pub fn observe(&mut self, remote: Hlc, physical_ms: u64) -> Hlc {
        let wall = physical_ms.max(self.last.wall_ms).max(remote.wall_ms);
        let counter = if wall == self.last.wall_ms && wall == remote.wall_ms {
            self.last.counter.max(remote.counter) + 1
        } else if wall == self.last.wall_ms {
            self.last.counter + 1
        } else if wall == remote.wall_ms {
            remote.counter + 1
        } else {
            0
        };
        self.last = Hlc {
            wall_ms: wall,
            counter,
        };
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn local_events_are_strictly_increasing_even_if_clock_goes_back() {
        let mut c = HlcClock::new();
        let a = c.now(1000);
        let b = c.now(900);
        let d = c.now(1000);
        assert!(a < b && b < d);
    }

    #[test]
    fn observe_moves_past_remote() {
        let mut c = HlcClock::new();
        c.now(1000);
        let remote = Hlc {
            wall_ms: 5000,
            counter: 7,
        };
        let after = c.observe(remote, 1001);
        assert!(after > remote);
        assert!(c.now(1002) > after);
    }

    proptest! {
        #[test]
        fn monotonic_under_arbitrary_skew(
            ops in prop::collection::vec((any::<bool>(), 0u64..10_000, 0u64..10_000, 0u32..5), 1..200)
        ) {
            let mut c = HlcClock::new();
            let mut prev = Hlc::default();
            for (is_remote, phys, rwall, rctr) in ops {
                let next = if is_remote {
                    let remote = Hlc { wall_ms: rwall, counter: rctr };
                    let t = c.observe(remote, phys);
                    prop_assert!(t > remote);
                    t
                } else {
                    c.now(phys)
                };
                prop_assert!(next > prev);
                prev = next;
            }
        }
    }
}
