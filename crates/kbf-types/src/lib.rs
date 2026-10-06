//! Shared value types for kbf: the content digest (function, hash, size), platform
//! properties, QoS levels, lease identifiers, farm time, constants, and the `StateMachine`
//! and `Effect` traits that the pure cores implement.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
