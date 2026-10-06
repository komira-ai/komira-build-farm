//! The metadata core: blob and action-cache records, the closure check, and the
//! present, absent and unavailable answers, as a pure state machine.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
