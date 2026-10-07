//! Worker capabilities: parsers over the text a node's OS publishes about its CPU
//! (Linux `/proc/cpuinfo`, macOS `sysctl hw.optional`), the ISA levels that text
//! implies, and matching of an action's capability request against a node.
//!
//! - [`CpuCaps`] holds a node's architecture, every feature it reports (kernel names)
//!   and its highest ISA level: an x86-64 psABI level ([`X86Level`]) or an Armv8
//!   version ([`ArmVersion`]).
//! - [`Request`] and [`NodeCaps`] match with typed comparisons: exact, at least for
//!   ordered levels, subset for feature sets, and minimums for countable resources.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod cpu;
mod level;
mod macos;
mod matching;

pub use cpu::{Arch, CpuCaps, ParseError, UnknownArch};
pub use level::{ArmVersion, IsaLevel, UnknownIsaLevel, X86Level};
pub use matching::{Consumable, NodeCaps, Request, RequestError, Unmet};
