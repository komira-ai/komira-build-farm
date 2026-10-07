//! The scheduler core: operation states, leases and fencing, result acceptance,
//! and assignment of actions to workers, as a pure state machine.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
