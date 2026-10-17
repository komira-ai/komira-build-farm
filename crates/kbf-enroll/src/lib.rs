//! The node's side of enrollment: a node holds a short-lived certificate for each role
//! the frontend granted it, made from a key that never left the node.
//!
//! - [`config`]: `kbf.toml`, the two required values and every other field with its
//!   validation, all problems reported together with their lines;
//! - [`token`]: the enroll token file, read only if nobody else could have read or
//!   written it;
//! - [`pin`]: first-contact trust, the server checked against the CA fingerprint the
//!   token carries (or, as a declared fallback, the system roots);
//! - [`client`]: `Enroll` over `kbf.enroll.v1`: one key per role, a CSR each, the token
//!   secret; the answer checked in [`answer`] before anything is written;
//! - [`store`]: keys, certificates and `node.json`, written atomically with their modes;
//! - [`renew`]: renewal at half life by a request signed by the registered key, with the
//!   schedule and backoff;
//! - [`keys`]: the object-store keys a storage node fetches over its mutual-TLS session
//!   and holds in memory only.
//!
//! This is a library; the `kbf enroll` and `kbf node` verbs that call it are another
//! crate's (`docs/design/install-and-enrollment.md`, unit B4b). Time is always a
//! parameter, so every decision is tested without a clock.

#![forbid(unsafe_code)]

pub mod answer;
pub mod client;
pub mod config;
mod csr;
pub mod keys;
pub mod pin;
pub mod renew;
pub mod store;
pub mod token;

pub use csr::ISSUED_AT_OID;

#[cfg(test)]
mod testutil;
