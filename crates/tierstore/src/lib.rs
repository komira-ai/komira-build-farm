//! A hot/cold object store over S3-compatible backends.
//!
//! The crate knows nothing of what is stored: objects are immutable byte strings under string
//! keys, and it depends on no crate of the workspace it lives in, so that it can move to a
//! repository of its own.
//!
//! - [`Backend`] is the minimal API of an S3-compatible store: a create-only put (it fails
//!   with [`Error::AlreadyExists`] where the key exists, as `If-None-Match: *` does), get with
//!   an optional byte range, head (size and content tag), delete, and list by prefix for
//!   recovery and garbage collection. Its methods are async, as the object store clients of
//!   the workspace are, and boxed so that a backend is used as `dyn Backend`.
//! - [`MemoryBackend`] is an in-memory backend, and [`FaultyBackend`] wraps a backend to fail
//!   or delay chosen calls: both are for tests.
//! - [`Tiered`] is a store over a hot and a cold backend that is itself a [`Backend`]: puts
//!   are acknowledged by the hot tier, a write-back copies objects to the cold tier, and a
//!   least-recently-used eviction frees the hot tier only of objects the cold tier holds.
//!   What is pending after a restart is found by listing the two tiers; there is no database.
//! - [`CachedStore`] answers repeated reads of an object's ranges from RAM in front of any
//!   backend, typically a [`Tiered`].
//!
//! Every tier and cache takes its settings as constructor parameters.

#![forbid(unsafe_code)]

mod backend;
mod cache;
mod error;
mod faulty;
mod memory;
mod state;
mod tiered;

pub use backend::{Backend, ByteRange, Listed, Meta};
pub use cache::CachedStore;
pub use error::Error;
pub use faulty::{FaultyBackend, Op};
pub use memory::MemoryBackend;
pub use tiered::{Background, EvictionReport, Maintenance, Tiered, WriteBackReport};
