//! Worker capabilities: parsers over the text a node's OS publishes about its CPU
//! (Linux `/proc/cpuinfo`, macOS `sysctl hw.optional`) and GPUs (Linux PCI functions
//! in sysfs), the ISA levels that text
//! implies, and matching of an action's capability request against a node.
//!
//! - [`CpuCaps`] holds a node's architecture, every feature it reports (kernel names)
//!   and its highest ISA level: an x86-64 psABI level ([`X86Level`]) or an Armv8
//!   version ([`ArmVersion`]).
//! - [`gpus_from_linux_pci`] and [`gpus_from_macos_sysctl`] count a node's GPUs, the
//!   `gpu` capacity a request books whole and exclusively.
//! - [`Request`] and [`NodeCaps`] match with typed comparisons: exact, at least for
//!   ordered levels, subset for feature sets, membership for sets a node reports
//!   (`xcode`), and minimums for countable resources.
//! - [`Request::from_platform`] reads an action's REAPI platform (`OSFamily`, `ISA`
//!   and kbf's own keys, the names in any case: [`property_name`]) as a request;
//!   [`NodeCaps::from_report`] reads a daemon's node report as the capabilities
//!   requests are matched against.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod cpu;
mod gpu;
mod level;
mod macos;
mod matching;
mod platform;
mod report;

pub use cpu::{Arch, CpuCaps, ParseError, UnknownArch};
pub use gpu::{PciFunction, gpus_from_linux_pci, gpus_from_macos_sysctl};
pub use level::{ArmVersion, IsaLevel, UnknownIsaLevel, X86Level};
pub use matching::{Consumable, NodeCaps, RESERVED_KEYS, Request, RequestError, Unmet};
pub use platform::{DAEMON_OSES, FromPlatformError, REAPI_KEYS, property_name};
pub use report::ReportError;
