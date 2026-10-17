//! Node rolling for a split-mode farm, as pure decisions (`docs/design/split-mode-updates.md`
//! sections 2.6 to 2.11; the design is #898): which host goes next ([`pick()`]), and the one
//! move a host makes next in its ten steps ([`step()`]).
//!
//! **Pure.** Nothing here reads a clock, a store or a socket, and nothing is async. Time
//! is an argument ([`kbf_types::FarmTime`]). The controller that follows fills [`Facts`]
//! from the `Farm`, `Members`, the hosts' heartbeats and the `update/*` rows, calls
//! [`pick()`] and [`step()`] on every look, records the phase (and the row fields) a
//! [`Transition`] names, and then runs its [`Command`]s. A restart re-reads the rows and
//! runs the entry commands of the phase it finds ([`HostRecord::entry_commands`]).
//!
//! **Rules the code keeps** (section 2.1, and the corrections made to the first plan by
//! review):
//! - One host in a step at a time. "Out" is read from the rows and the slot the
//!   controller wrote, never from member state, which lags.
//! - Every gate fails closed. An absent metric is `None`, never zero.
//! - A planned restart is `MarkDown`, never `Draining` (which starts a handoff of the
//!   node's slice).
//! - Capacity beats update: a node is cordoned only if `min_serving` other hosts of its
//!   platform stay; steps 2 to 5 give the host back on any failure, and the lease wait
//!   ends at its limit (15 minutes on Linux, 60 on a Mac); from the activate order on
//!   nothing uncordons the host until step 10.
//! - A failure holds the run; it is not retried and the host is not skipped.
//! - A rollback is another target with a higher serial, walked by the same code, and it
//!   walks back the hosts the forward run finished.
//! - A helper's status counts only if it names the host's current activation order, and a
//!   restart counts only with a larger boot counter than the one recorded at the mark.
//!
//! **What is not here.** The controller, the `update/*` rows, the transport to the helpers
//! and every metric the facts are filled from. The picker starts one host at a time even
//! for daemon-only hosts, which the design lets run in parallel up to the capacity floor.

mod command;
mod facts;
mod gate;
mod node_step;
mod pick;
mod policy;
mod steps;

pub use command::{Alert, Command, GiveBack, HoldReason, Severity, SlotOp};
pub use facts::{
    DaemonLoad, Facts, HelperReport, HelperStatus, HostFacts, HostKind, HostRecord, MemberFacts,
    Observations, OrderId, Platform, PlatformClass, Release, Serial, SlotView, Work,
};
pub use gate::{GateFail, GateMode, storage_gate};
pub use node_step::{HostPhase, Transition, step};
pub use pick::{Pick, Skip, WaitReason, pick};
pub use policy::{Policy, PolicyError};
