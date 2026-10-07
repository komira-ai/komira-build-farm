//! The scheduler core: operation states, leases and fencing, result acceptance,
//! and assignment of actions to workers, as a pure state machine.
//!
//! [`Scheduler`] is a [`kbf_types::StateMachine`]: the caller feeds it [`Input`]s
//! (submissions, worker reports, committed control-log records, ticks), each carrying
//! the farm time, and carries out the [`kbf_types::Effect`]s it returns in order:
//! commit a record to the control log, send a `Start` to a worker, answer waiters.
//!
//! What v0 covers (RFC section 5):
//!
//! - operation states `Queued -> Leased -> Running -> Completed | Failed`;
//! - commit before Start, and one accepted result per operation, fenced by lease id
//!   `(term, seq)` (see [`Scheduler`]);
//! - re-dispatch after the lease grace G and the worker's self-fence T ([`fence`]);
//! - reconciliation of held leases with the running set each worker's heartbeats
//!   send: a lease they leave out is requeued at once if its `Start` went to a session
//!   before the worker registered again, else once its `Start` has been out for G;
//! - the infra retry budget: each lease lost after its `Start` went out is one `INFRA`
//!   attempt, retried on a worker that has not lost the operation when one has room,
//!   and the third fails the operation with an `INFRA` result (RFC section 5.8);
//! - a lost lease its worker still lists as running stays booked on that worker until
//!   it leaves the running set;
//! - first-fit placement of a CPU and memory request onto worker capacity;
//! - in-flight dedup by instance and action digest, with waiters attached to one
//!   operation;
//! - QoS levels ordering the queue (no quotas).
//!
//! Not yet: placement scoring (alignment, best fit), capability matching, reclaimed
//! room and preemption, a worker's own `INFRA` report counted against the budget (it
//! fails the operation at once), and committing submissions so that a new leader
//! inherits the queue. In v0 the scheduler runs on the leader only.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod fence;
mod input;
mod scheduler;

pub use fence::SelfFence;
pub use input::{Event, Input, Request};
pub use scheduler::{INFRA_ATTEMPTS, OpState, PLACEMENT_ROUND, Scheduler};
