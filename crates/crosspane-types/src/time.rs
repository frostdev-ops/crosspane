//! Monotonic time.
//!
//! Every node has one monotonic clock with an arbitrary epoch (03 §8). Times from different nodes
//! are never compared directly; protocol behaviour never relies on cross-node clock agreement.

use core::time::Duration;

use serde::{Deserialize, Serialize};

/// A point on this node's monotonic clock, in nanoseconds since an arbitrary per-node epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonoTime(u64);

impl MonoTime {
    /// The clock's epoch.
    pub const ZERO: MonoTime = MonoTime(0);

    pub const fn from_nanos(nanos: u64) -> Self {
        MonoTime(nanos)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// `self + d`, or `None` on overflow.
    pub fn checked_add(self, d: Duration) -> Option<Self> {
        let d = u64::try_from(d.as_nanos()).ok()?;
        self.0.checked_add(d).map(MonoTime)
    }

    /// `self + d`, saturating at the end of time.
    pub fn saturating_add(self, d: Duration) -> Self {
        self.checked_add(d).unwrap_or(MonoTime(u64::MAX))
    }

    /// Time elapsed from `earlier` to `self`, or zero if `earlier` is later.
    pub fn saturating_duration_since(self, earlier: MonoTime) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// A source of monotonic time. Platform crates read the OS clock; `crosspane-testkit` provides a
/// deterministic clock for simulations.
pub trait Clock: Send + Sync {
    fn now(&self) -> MonoTime;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let t = MonoTime::from_nanos(1_000);
        assert_eq!(
            t.checked_add(Duration::from_nanos(500)),
            Some(MonoTime::from_nanos(1_500))
        );
        assert_eq!(
            MonoTime::from_nanos(u64::MAX).checked_add(Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            MonoTime::from_nanos(u64::MAX - 1).saturating_add(Duration::from_secs(1)),
            MonoTime::from_nanos(u64::MAX)
        );
        assert_eq!(
            MonoTime::from_nanos(1_500).saturating_duration_since(t),
            Duration::from_nanos(500)
        );
        assert_eq!(
            t.saturating_duration_since(MonoTime::from_nanos(1_500)),
            Duration::ZERO
        );
    }
}
