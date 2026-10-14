//! `kbf-transfer`: the shared blob transfer engine of a kbf-daemon
//! (`docs/design/worker-cache.md#4-the-shared-transfer-engine`).
//!
//! Every blob a daemon moves goes through one [`Transfers`] per process: input files,
//! output uploads, and the input cache's fills. It speaks gRPC to kbf-server over one
//! HTTP/2 connection with many streams, and bounds the whole process with one
//! in-flight limit and one byte budget, not one per call. Uploads hold at most all but
//! a quarter of the limit, so a slow store under one lease's upload leaves permits for
//! every lease's reads. Concurrent reads of one
//! digest, from any lease, share one fetch. Every upload is preceded by FindMissing (its
//! own, or one call for a whole set of blobs, [`Cas::missing`]), so a blob the server
//! holds is not sent again. A read broken by a reset connection
//! resumes from the bytes it received.
//!
//! - [`cas`]: the [`Cas`] trait every store implements (the engine, and the in-memory
//!   stores of tests), with [`digest_of`], [`fetch`] and [`label`].
//! - [`Transfers`] and [`Scope`]: the engine, and one lease's view of it, which counts
//!   the lease's [`TransferStats`].
//! - [`TransferConfig`]: the `--transfer-*` flags and `--segment-upload-streams`.
//! - [`Spool`]: an action's outputs gathered in one local file, which
//!   [`Cas::put_spool`] stores; the engine sends them as whole segments over
//!   `kbf.worker.v1.Segments` (`docs/design/write-path.md`, section 4), blob by blob
//!   when the server cannot take them.
//!
//! The engine's core (the permits, the byte budget, the shared fetches and the counters)
//! is in `engine` and `flights`; the REAPI backend (ByteStream and FindMissing calls)
//! is `wire`. Batching small blobs (BatchReadBlobs, BatchUpdateBlobs) is stage T2 and
//! not here.
//!
//! The engine has no TLS policy: it connects to whatever endpoint it is given
//! (`docs/design/worker-cache.md#47-tls`).

pub mod cas;
mod config;
mod engine;
mod find;
mod flights;
mod segments;
mod spool;
mod stats;
mod wire;

pub use cas::{Cas, CasError, Chunks, WRITE_CHUNK_BYTES, digest_of, fetch, label};
pub use config::{DEFAULT_IN_FLIGHT, DEFAULT_SEGMENT_STREAMS, TransferConfig};
pub use engine::{Scope, Transfers};
pub use segments::{
    COALESCE_GAP_BYTES, Located, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS, SEGMENT_UPLOAD_BYTES,
};
pub use spool::{Spool, Spooled};
pub use stats::{EngineStats, TransferStats};
pub use wire::{MAX_CONTROL_MESSAGE_BYTES, MAX_MESSAGE_BYTES};
