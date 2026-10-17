//! The store's clock. Expiry is judged by the store, never by a caller, so a backend owns
//! a [`Clock`] and tests inject a [`ManualClock`] they advance by hand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A reading of a store's clock: the time since that clock's origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreTime(Duration);

impl StoreTime {
    /// The origin of a clock.
    pub const ZERO: StoreTime = StoreTime(Duration::ZERO);

    /// The reading `since_origin` after the clock's origin.
    #[must_use]
    pub const fn from_duration(since_origin: Duration) -> StoreTime {
        StoreTime(since_origin)
    }

    /// The time since the clock's origin.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// This reading moved `by` later, saturating at the largest representable time.
    #[must_use]
    pub fn saturating_add(self, by: Duration) -> StoreTime {
        StoreTime(self.0.saturating_add(by))
    }

    /// How long after `earlier` this reading is; zero when `earlier` is not earlier.
    #[must_use]
    pub fn since(self, earlier: StoreTime) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// A source of [`StoreTime`]. Never goes backwards.
pub trait Clock: Send + Sync {
    /// The current reading.
    fn now(&self) -> StoreTime;
}

/// A clock that moves only when told to. Shared by `Arc` between a test and the store
/// it is injected into.
#[derive(Debug, Default)]
pub struct ManualClock {
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    /// Moves the clock forward by `by` (saturating).
    pub fn advance(&self, by: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap_or_else(PoisonError::into_inner);
        *elapsed = elapsed.saturating_add(by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> StoreTime {
        StoreTime(*self.elapsed.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The wall clock, with the Unix epoch as its origin, so a reading means the same after a
/// restart (a store that keeps expiry times on disk needs that). It never goes backwards:
/// a reading below an earlier one (the system clock was set back) repeats the earlier one.
#[derive(Debug, Default)]
pub struct SystemClock {
    latest_nanos: AtomicU64,
}

impl Clock for SystemClock {
    fn now(&self) -> StoreTime {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let nanos = u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX);
        let latest = self
            .latest_nanos
            .fetch_max(nanos, Ordering::AcqRel)
            .max(nanos);
        StoreTime(Duration::from_nanos(latest))
    }
}
