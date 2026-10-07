//! Shared value types for kbf: the content digest (function, hash, size), platform
//! properties, QoS levels, lease identifiers, farm time, and the `StateMachine` trait
//! and `Effect` enum that the pure cores implement and emit.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.
//! Every value here is plain data: two processes that build a value from the same
//! inputs get equal values that order and format identically.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod digest;
mod lease;
mod platform;
mod qos;
mod state;
mod time;

pub use digest::{Digest, DigestFunction, ParseDigestError};
pub use lease::LeaseId;
pub use platform::{Platform, PlatformError};
pub use qos::{CustomQos, Qos, QosError};
pub use state::{Effect, StateMachine};
pub use time::FarmTime;
