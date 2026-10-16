//! Whole-cell simulation: a server, daemons, clients and an object store assembled on
//! the `kbf-sim` kernel and driven by seeds (docs/design/simulation.md, "The cell").
//!
//! This is the one-server skeleton of docs/design/ha.md section 15. The server
//! ([`Server`]) is assembled from the code `kbf-server` runs: the
//! metadata log is a [`kbf_server::RaftMetaLog`] (a single-voter `kbf-raft` host over
//! a `kbf-store` store, its entries `kbf-log` commands applied to a `MetaState`) on a
//! fault-injecting in-memory disk ([`kbf_store::FaultFs`]); the farm core is a
//! `kbf-sched` [`kbf_sched::Scheduler`] and a [`kbf_server::machine::FarmMachine`],
//! fed as `kbf-server`'s farm core feeds them. Daemons ([`FakeDaemon`]) keep their
//! contact with the server in the daemon's own [`kbf_daemon::Contact`] and drop the
//! leases of an earlier epoch by [`kbf_daemon::superseded`]. Clients ([`FakeClient`])
//! upload an input, check it with `FindMissingBlobs` and `Execute` an action. The
//! object store ([`ObjectStore`]) keeps every object it accepts, and meets the faults
//! of its [`StorePlan`]: it hangs, is slow, fails PUTs, or loses their answers. The
//! server can be taken down at a named point of its write path ([`CrashPlan`]).
//!
//! **All-in-one** ([`Kind::WholeProcess`]): one daemon is the daemon half of the
//! server's process (`kbf-all-in-one`; its design, komira-build-farm#657, section 8).
//! It stores each run's output in the server before it reports, and a kill of the
//! process takes both halves down together, mid-build, and starts both again.
//!
//! [`run`] runs a [`Scenario`] step by step and checks properties 1 to 7 of ha.md
//! section 15, the write path's 8 and 9 (docs/design/write-tier-scenarios.md), and
//! all-in-one's 14, after every step ([`check`]); a violation panics with the
//! scenario, the seed and the property. A seed replays to the same trace. [`sweep`]
//! runs a scenario per seed and reports every failing seed.

mod cell;
pub mod check;
mod client;
mod daemon;
mod hook;
mod replica;
mod server;
mod store;
mod sweep;

use kbf_meta::ObjectId;
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, Outcome, Resources};

pub use cell::{Kind, RunReport, Scenario, run};
pub use client::{FakeClient, Job};
pub use daemon::FakeDaemon;
pub use hook::{CrashPlan, Down, StorePoint, TierPoint};
pub use server::{
    Cut, PUT_TIMEOUT, Seen, Server, ServerFault, ServerPlan, TierCut, TierPlan, TierSeen,
};
pub use store::{BlobHomes, Body, Held, Met, ObjectStore, StoreFault, StorePlan, StoreStats};
pub use sweep::{Failure, Sweep, sweep};

/// What the nodes of a cell send each other. `stream` stands for a daemon's gRPC
/// stream: a daemon numbers its streams, and a new one replaces the old.
#[derive(Clone, Debug)]
pub enum Msg {
    /// Client (or the in-process daemon half, an action's output) to server: store
    /// this blob (the bytes travel with it).
    Upload {
        digest: Digest,
    },
    /// Server to client (or daemon half): the blob is stored and indexed.
    Uploaded {
        digest: Digest,
    },
    /// Server to client: the store refused the blob's PUT; nothing is indexed
    /// (`UNAVAILABLE`). The client tries again later.
    UploadFailed {
        digest: Digest,
    },
    /// Client to server: `FindMissingBlobs`.
    FindMissing {
        digests: Vec<Digest>,
    },
    /// Server to client: of `asked`, these are missing.
    Missing {
        asked: Vec<Digest>,
        missing: Vec<Digest>,
    },
    /// Client to server: `Execute`.
    Execute {
        action: Digest,
    },
    /// Server to client: the action's answer; `cached` if from the action cache.
    Answered {
        action: Digest,
        cached: bool,
    },
    /// Server to object store: write `object`, holding `digest`.
    Put {
        object: ObjectId,
        digest: Digest,
    },
    /// Object store to server: `object` is written.
    PutDone {
        object: ObjectId,
        digest: Digest,
    },
    /// Object store to server: the PUT of `object` failed. (The store may have kept it
    /// all the same: [`StoreFault::KeepButFail`].)
    PutFailed {
        object: ObjectId,
        digest: Digest,
    },
    /// Server to object store: write the flushed segment `object` of the local write
    /// tier.
    Flush {
        object: ObjectId,
        body: Body,
    },
    /// Object store to server: the segment `object` is written.
    FlushDone {
        object: ObjectId,
    },
    /// Object store to server: the PUT of the segment `object` failed.
    FlushFailed {
        object: ObjectId,
    },
    /// Server to object store: read `digest` from `object` (a reader that did not find
    /// it on the local tier); `tag` names the read.
    Get {
        object: ObjectId,
        digest: Digest,
        tag: u64,
    },
    /// Object store to server: whether `object` held the blob the read `tag` asked for.
    Got {
        tag: u64,
        held: bool,
    },
    /// Daemon to server: the first message of a stream.
    Hello {
        stream: u64,
        instance: String,
        capacity: Resources,
    },
    /// Server to daemon: the answer to a stream's first `Hello`.
    Welcome {
        stream: u64,
        epoch: u64,
        interval_ms: u64,
    },
    Heartbeat {
        stream: u64,
        seq: u64,
        running: Vec<LeaseId>,
    },
    HeartbeatAck {
        stream: u64,
        seq: u64,
    },
    Start {
        stream: u64,
        lease: LeaseId,
        operation: OperationId,
        action: Digest,
    },
    /// Daemon to server: a lease's result, naming the action its `Start` named.
    Report {
        stream: u64,
        lease: LeaseId,
        action: Digest,
        outcome: Outcome,
    },
    ResultAck {
        lease: LeaseId,
        accepted: bool,
    },
    Cancel {
        stream: u64,
        lease: LeaseId,
    },
    /// Server to daemon: this server holds no such stream (it restarted); open another.
    Reset {
        stream: u64,
    },
}

/// The action digest of action `n`.
#[must_use]
pub fn action(n: u64) -> Digest {
    numbered(0xAC, n)
}

/// The input blob of `action`, which a run of it also names as its `ActionResult`, so
/// an action-cache entry is servable without a second upload.
#[must_use]
pub fn input_of(action: &Digest) -> Digest {
    let mut input = *action;
    input.hash[0] = 0x1B;
    input
}

/// The bytes of the blob `digest` names, as the cell's clients upload them: its size,
/// each byte drawn from the digest. (The cell's digests are names, not hashes of
/// these bytes; a reader checks bytes against this.)
#[must_use]
pub fn content(digest: &Digest) -> Vec<u8> {
    let len = usize::try_from(digest.size_bytes).unwrap_or(0);
    (0..len)
        .map(|i| digest.hash[i % 32] ^ u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

/// The output blob a run of `action` by the in-process daemon half stores, which its
/// result names as its `ActionResult`.
#[must_use]
pub fn output_of(action: &Digest) -> Digest {
    let mut output = *action;
    output.hash[0] = 0x0B;
    output
}

fn numbered(tag: u8, n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[0] = tag;
    hash[8..16].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n + 1)
}
