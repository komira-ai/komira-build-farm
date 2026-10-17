//! Node rolling for a split-mode farm (`docs/design/split-mode-updates.md` sections 2.6 to
//! 2.11, 5.3 and 6.1; the design is #898): which hosts go next ([`pick()`]), the one move a
//! host makes next in its ten steps ([`step()`]), the durable rows both are driven from
//! ([`Rows`]) and the [`Controller`] that drives them.
//!
//! **Pure decisions.** [`pick()`] and [`step()`] read nothing and write nothing: no clock, no
//! store, no socket, nothing async. Time is an argument ([`kbf_types::FarmTime`]). The
//! controller fills [`Facts`] from the `Farm`, `Members`, the hosts' heartbeats (through
//! [`FactsSource`]) and the `update/*` rows, calls them on every look, records the phase (and
//! the row fields) a [`Transition`] names, and then gives its [`Command`]s (through
//! [`Transport`]). A restart re-reads the rows and gives the entry commands of the phase it
//! finds ([`HostRecord::entry_commands`]).
//!
//! **Rules the code keeps** (section 2.1, and the corrections made to the first plan by
//! review):
//! - One storage host in a step at a time, and up to [`Policy::max_hosts_out`] daemon-only
//!   hosts and Macs beside it. "Out" is read from the rows and the slot the controller
//!   wrote, never from member state, which lags.
//! - Every gate fails closed. An absent metric is `None`, never zero.
//! - A planned restart is `MarkDown`, never `Draining` (which starts a handoff of the
//!   node's slice).
//! - Capacity beats update: a node is cordoned only if `min_serving` other hosts of its
//!   platform stay; steps 2 to 5 give the host back on any failure, and the lease wait
//!   ends at its limit (15 minutes on Linux, 60 on a Mac); from the activate order on
//!   nothing uncordons the host until step 10.
//! - A failure holds the run; it is not retried and the host is not skipped.
//! - A rollback is another target under a higher generation, walked by the same code, and it
//!   walks back the hosts the forward run finished. A host that has done nothing
//!   irreversible yet (steps 1 to 5) is given back when a new generation is offered; a host
//!   past the activate order finishes the step it began.
//! - A helper's status counts only if it names the host's current activation order, and a
//!   restart counts only with a larger boot counter than the one recorded at the mark.
//! - The row is written before the orders it calls for, and never over a row that moved. The
//!   exception is a host that leaves its step: the orders that undo the step come first
//!   ([`Controller`]).
//!
//! **What is not here.** The farm: no [`Transport`] to the helpers, no [`FactsSource`] over
//! the `Farm`, the members and the heartbeats, no metrics, and no call from the frontend.
//! Nothing outside the tests runs a [`Controller`] yet.

mod codec;
mod command;
mod controller;
mod facts;
mod gate;
mod moves;
mod node_step;
mod pick;
mod policy;
mod ports;
mod report;
mod rows;
mod slot;
mod steps;

pub use codec::{Desired, HostRow};
pub use command::{Alert, Command, GiveBack, HoldReason, Severity, SlotOp};
pub use controller::Controller;
pub use facts::{
    DaemonLoad, Facts, Generation, HelperReport, HelperStatus, HostFacts, HostKind, HostRecord,
    MemberFacts, Observations, OrderId, Platform, PlatformClass, Release, SlotView, Work,
};
pub use gate::{GateFail, GateMode, storage_gate};
pub use node_step::{HostPhase, Transition, step};
pub use pick::{Pick, Picked, Skip, WaitReason, pick};
pub use policy::{Policy, PolicyError};
pub use ports::{FactsSource, FarmView, SourceError, Transport, TransportError};
pub use report::{CommandError, ControllerError, Moved, Tick};
pub use rows::{RowError, Rows, Stored};
pub use slot::{Holder, Slot, SlotClass};
