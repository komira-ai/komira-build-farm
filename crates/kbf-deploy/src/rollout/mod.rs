//! The `rollout` kit: a new kbf commit put on the whole fleet, one host at a time, with
//! every gate checked and every failure undone. The logic never touches a host itself.
//! It asks a [`Transport`] to, so the same rollout runs against the simulator of the
//! tests and against the real hosts.
//!
//! - [`inventory`]: the hosts and their roles, and the order a rollout takes them in:
//!   every storage-and-daemon node, then the frontend, then the Linux daemons, then the
//!   Macs.
//! - [`plan`]: the steps of a rollout, printed by a dry run and announced by an apply.
//! - [`transport`]: the [`Transport`] trait, the contract between the rollout and the
//!   host verbs (`stage`, `drain`, `swap`, `confirm`, `undrain`, `rollback`), and the
//!   names that cross it.
//! - [`engine`]: the state machine ([`engine::run`]): the fleet lock, the state-store
//!   backup, then per host the gate, stage, drain, swap, health wait, confirm and
//!   undrain, with automatic rollback of the host that failed and a stop right there.
//! - [`cli`]: `kbf-deploy rollout`.

pub mod cli;
pub mod engine;
pub mod inventory;
pub mod plan;
pub mod transport;

pub use engine::{Limits, Outcome, RolloutError, run};
pub use inventory::{Inventory, Role};
pub use plan::Plan;
pub use transport::{Commit, HostName, RunId, Transport};

#[cfg(test)]
mod fleet_tests;
