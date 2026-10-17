//! Read repair: what a router does when a read finds a blob somewhere other than where
//! it should be.
//!
//! A read that is answered by an owner after an earlier owner of the same generation
//! said it did not hold the blob has found a copy missing. The router tells a
//! [`ReadRepair`] hook (the repair side, av2-11, implements it) which node answered and
//! which owners lacked the blob, and goes on serving the read: the hook only queues. The
//! router rate-limits the hook ([`RepairLimit`]) because a cold owner during a join
//! answers NOT_FOUND for most of what it is asked, and every such read would otherwise
//! be a repair.

use std::num::NonZeroU32;
use std::sync::Mutex;
use std::sync::PoisonError;

use kbf_map::MemberId;
use kbf_types::Digest;
use tokio::time::Instant;

/// The repair side's entry point.
pub trait ReadRepair: Send + Sync {
    /// A read of `digest` was answered by `found_at` after the owners `missing_at`, of
    /// the map the read was routed by, answered that they did not hold it (or held it
    /// unreadably, or sent bytes that are not the blob). Returns at once: it queues, it never copies the blob inline.
    fn found_elsewhere(&self, digest: &Digest, found_at: MemberId, missing_at: &[MemberId]);
}

/// How many times the hook may be called: `burst` at once, then `per_second`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairLimit {
    /// The most calls that may come together.
    pub burst: NonZeroU32,
    /// How many calls the limit gives back each second.
    pub per_second: NonZeroU32,
}

/// A token bucket on the runtime's clock (so a test with a paused clock decides it).
#[derive(Debug)]
pub(crate) struct Limiter {
    limit: RepairLimit,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Limiter {
    pub(crate) fn new(limit: RepairLimit) -> Self {
        Self {
            limit,
            state: Mutex::new(Bucket {
                tokens: f64::from(limit.burst.get()),
                at: Instant::now(),
            }),
        }
    }

    /// Whether a call may go ahead now; takes its token if so.
    pub(crate) fn allow(&self) -> bool {
        let now = Instant::now();
        let mut bucket = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.at = now;
        let full = f64::from(self.limit.burst.get());
        bucket.tokens =
            (bucket.tokens + elapsed * f64::from(self.limit.per_second.get())).min(full);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return true;
        }
        false
    }
}
