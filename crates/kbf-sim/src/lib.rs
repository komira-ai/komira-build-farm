//! The deterministic simulation kernel: a virtual clock, a seeded random stream, a
//! message bus between named nodes with delay, drop, duplicate, reorder and partition
//! faults, a run loop that drives [`kbf_types::StateMachine`] nodes, and a trace whose
//! hash identifies the run.
//!
//! A run is a function of its seed and its scenario: [`Sim`] reads no clock and no
//! operating-system entropy and keeps every collection ordered, so the same seed gives
//! the same [`TraceHash`] in any process on any platform. A failing seed is a complete
//! bug report. The kernel depends only on `kbf-types`; pure crates use it as a
//! dev-dependency only, and the whole-cell assembly lives in `kbf-sim-cell`.
//!
//! The kernel holds itself to the pure-crate rules (no clock, sleep, network or hashed
//! collections; see `clippy.toml`) because breaking any of them breaks replay.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod net;
mod node;
mod rng;
mod sim;
mod trace;

pub use net::{Faults, NodeId, Partition};
pub use node::{Event, Node, NodeInput, Output};
pub use rng::{Chance, SimRng};
pub use sim::Sim;
pub use trace::TraceHash;
