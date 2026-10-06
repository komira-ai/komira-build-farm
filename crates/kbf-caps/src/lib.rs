//! Worker capabilities: parsers over capability strings (CPU flags, ISA levels,
//! hwcaps), matching of action platform requirements against them, and pool aliases.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
