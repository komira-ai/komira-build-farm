//! The scheduler core: operation states, leases and fencing, result acceptance,
//! and assignment of actions to workers, as a pure state machine.
//!
//! [`Scheduler`] is a [`kbf_types::StateMachine`]: the caller feeds it [`Input`]s
//! (submissions, worker reports, committed control-log records, ticks), each carrying
//! the farm time, and carries out the [`kbf_types::Effect`]s it returns in order:
//! commit a record to the control log, send a `Start` to a worker, answer waiters.
//!
//! What v0 covers (`docs/design/scheduler.md`):
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
//!   lists a driver serving the lease kind and satisfies the action's platform
//!   (`kbf-caps` matching);
//! - whole-machine leases: one goes only to a worker that holds no lease, and books
//!   all of it, so nothing is placed beside it; one that fits nowhere holds a worker
//!   that could run it, in queue (QoS) order, and less urgent work is not placed there
//!   until it has emptied;
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
//!   answered, then dropped (issue #165);
//! - memory kills (failure classes, 6.1): an action that passed its own memory limit
//!   runs again with its memory booking doubled, up to the largest node that could run
//!   it, and its action key keeps the raised booking as a floor for later submissions;
//!   a kill at that largest node finishes it. A busy node's kill below the action's
//!   own limit runs it again with the same booking, at most [`FARM_RERUNS`] times, and
//!   counts against the node ([`Scheduler::memory_pressure`]). See
//!   [`kbf_types::MemoryRun`].
//!
//! Not yet: placement scoring (alignment, best fit), a reservation for a large
//! `action` request (issue #169), reclaimed
//! room and preemption, the infra retry budget for other farm faults (a
//! [`kbf_types::Failure::Infra`] result still finishes its operation), and committing
//! submissions so that a new leader inherits the queue. In v0 the scheduler runs on
//! the leader only.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod cordon;
pub mod fence;
mod input;
mod requeue;
mod scheduler;
mod servable;

pub use cordon::Cordon;
pub use fence::SelfFence;
pub use input::{DaemonInstance, Event, Input, Request};
pub use requeue::{Gib, Requeue, RequeueReason};
pub use scheduler::memory::{FARM_RERUNS, MEMORY_FLOORS, raised};
pub use scheduler::{FINISHED_RETENTION, OpState, PLACEMENT_ROUND, Scheduler, UNSERVABLE_WAIT};
