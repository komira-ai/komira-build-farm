//! The farm's state store: the trait the scheduler writes every change through, an
//! in-memory fake, and the conformance suite every backend must pass.
//!
//! The contract is section 4.1 of `docs/design/architecture-v2.md`:
//!
//! - **C1** compare-and-set per key with a version ([`Cond`], [`Version`]);
//! - **C2** atomic multi-key transactions ([`Txn`]);
//! - **C3** strictly monotonic counters ([`Op::Next`]);
//! - **C4** time-to-live keys expired by the store's own [`Clock`], observable in the
//!   feed ([`Event::Expire`]);
//! - **C5** ordered prefix scans and a change feed a poller resumes from a [`Revision`]
//!   without skipping a row ([`Store::changes`]);
//! - **C6** an acknowledgement is durable across restart;
//! - **C7** group commit ([`Store::apply_batch`], [`GROUP_COMMIT_WINDOW`]);
//! - **C8** a bounded, ordered range scan under a prefix, between two bounds, forward or
//!   in reverse, that pages with a [`ResumeToken`] ([`Store::scan_range`], [`ScanRange`]).
//!
//! **Fencing** ([`Fenced`]): a writer takes the store's generation, one row, when its
//! role begins, and every write it makes checks the row in the same transaction. A
//! writer that began later makes every write of an earlier one fail, so two never act.
//!
//! With the `postgres` feature, [`postgres`] is the Postgres backend.
//!
//! With the `test-support` feature (and in this crate's own unit tests) `conformance`
//! states each clause as a function generic over a `conformance::Backend` factory, and
//! `fake` is the in-memory backend, with a deliberately broken variant per clause that
//! the suite must reject. A default build has neither: a crate that tests against the
//! fake enables the feature in its `[dev-dependencies]`.

pub mod clock;
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
mod fence;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sqlite;
mod store;
mod types;

use std::time::Duration;

pub use clock::{Clock, ManualClock, StoreTime, SystemClock};
pub use fence::{Fenced, GENERATION, TakeError};
pub use store::Store;
pub use types::{
    Change, Changes, Commit, CommitStats, Cond, Conflict, Direction, Event, Op, ResumeToken,
    Revision, ScanPage, ScanRange, StoreError, Txn, Version, Versioned,
};

/// The longest a write may wait for the group it commits with (target 5 ms).
pub const GROUP_COMMIT_WINDOW: Duration = Duration::from_millis(5);

/// The write rate the store must sustain in bursts: a burst of 300 actions a second at
/// about five durable writes each.
pub const TARGET_WRITES_PER_SECOND: u64 = 1500;
