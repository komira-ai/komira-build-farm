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
//!   before the same daemon process registered again, after the handover grace if it
//!   went to another daemon process registered as the worker (issue #140), else once
//!   its `Start` has been out for G;
//! - first-fit placement of a CPU, memory and GPU request onto worker capacity, GPUs
//!   whole and held by one lease each until it ends, on workers whose node report
//!   satisfies the action's platform (`kbf-caps` matching);
//! - work no live worker can run (none satisfies its platform, or none that does is
//!   large enough) waits with a reason its callers see, and is refused after
//!   [`UNSERVABLE_WAIT`], the refusal committed before its callers are answered;
//! - in-flight dedup by instance and action digest, with waiters attached to one
//!   operation;
//! - QoS levels ordering the queue (no quotas);
//! - cordon and drain ([`Cordon`]): placement skips a cordoned worker, whose leases run
//!   on; a drain waits for them until a deadline, then pauses, and never kills. Work
//!   only cordoned workers could run waits, naming them, and is not refused for it; an
//!   uncordon places queued work at once;
//! - a finished operation is kept for [`FINISHED_RETENTION`] after its waiters are
//!   answered, then dropped (issue #165).
//!
//! Not yet: placement scoring (alignment, best fit), reclaimed
//! room and preemption, the infra retry budget, and committing submissions so that a
//! new leader inherits the queue. In v0 the scheduler runs on the leader only.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod cordon;
pub mod fence;
mod input;
mod scheduler;
mod servable;

pub use cordon::Cordon;
pub use fence::SelfFence;
pub use input::{DaemonInstance, Event, Input, Request};
pub use scheduler::{FINISHED_RETENTION, OpState, PLACEMENT_ROUND, Scheduler, UNSERVABLE_WAIT};
