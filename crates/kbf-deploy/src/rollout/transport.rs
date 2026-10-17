//! What the rollout asks of a fleet: the [`Transport`] trait, and the names and answers
//! that cross it.
//!
//! The rollout ([`super::engine`]) knows the order and the gates, and nothing about how
//! a host is reached. Everything it does to the fleet it does through this trait, one
//! call per host verb, so the same rollout runs against the simulator in the tests and
//! against the real hosts. The real transport (ssh over the tailnet, one forced command
//! per call) and the host side of each verb (`kbf-deploy host stage|drain|swap|confirm|
//! undrain|rollback`) are built against this module, so its docs are the contract.
//!
//! Rules every implementation keeps:
//!
//! - A call returns when the verb is done on the host, or says why it is not. `Ok`
//!   means done, `Err` means the host may be in either state (a reply lost after the
//!   work was done is an `Err`), which is why the rollout answers an error after
//!   [`Transport::swap`] with [`Transport::rollback`] and never with a retry.
//! - [`Transport::drain`], [`Transport::undrain`], [`Transport::confirm`] and
//!   [`Transport::rollback`] are idempotent: calling one twice is the same as once.
//! - Nothing here blocks to wait for a condition. The rollout polls
//!   [`Transport::in_flight`], [`Transport::check`] and
//!   [`Transport::under_replicated_blobs`] and passes the time between polls to
//!   [`Transport::sleep`], so the simulator can move its own clock instead of the
//!   process's.
//! - A transport never kills a lease or an Execute stream: a kill spends the
//!   scheduler's loss budget. Draining only stops new work.

use std::fmt;
use std::time::Duration;

/// Why a name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{what} {value:?} is not valid: {rule}")]
pub struct NameError {
    what: &'static str,
    value: String,
    rule: &'static str,
}

/// A host's name in the inventory: 1 to 63 characters of `a-z`, `0-9` and `-`, starting
/// with a letter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostName(String);

impl HostName {
    pub fn new(name: &str) -> Result<HostName, NameError> {
        let mut chars = name.chars();
        let first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if first && rest && name.len() <= 63 {
            Ok(HostName(name.to_owned()))
        } else {
            Err(NameError {
                what: "host name",
                value: name.to_owned(),
                rule: "1 to 63 characters of a-z, 0-9 and -, starting with a letter",
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The commit a rollout puts on the hosts: 40 lower-case hexadecimal digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Commit(String);

impl Commit {
    pub fn new(sha: &str) -> Result<Commit, NameError> {
        let hex = sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex {
            Ok(Commit(sha.to_owned()))
        } else {
            Err(NameError {
                what: "commit",
                value: sha.to_owned(),
                rule: "40 lower-case hexadecimal digits",
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who is rolling: the name a rollout holds the fleet lock under, 1 to 64 characters of
/// `A-Z`, `a-z`, `0-9`, `.`, `_` and `-`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId(String);

impl RunId {
    pub fn new(id: &str) -> Result<RunId, NameError> {
        let chars = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if chars && !id.is_empty() && id.len() <= 64 {
            Ok(RunId(id.to_owned()))
        } else {
            Err(NameError {
                what: "run id",
                value: id.to_owned(),
                rule: "1 to 64 characters of A-Z, a-z, 0-9, . _ and -",
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The answer to [`Transport::lock`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lock {
    /// This run holds the lock now.
    Acquired,
    /// Another rollout holds it; nothing changed.
    Held { by: RunId },
}

/// What one look at a host's upgraded process found ([`Transport::check`]). Healthy is
/// all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Health {
    /// The host's kbf unit is active (a crash loop is not).
    pub unit_active: bool,
    /// The process answers `/readyz`.
    pub ready: bool,
    /// The farm lists the host as `active`, registered by the commit asked about (a
    /// binary that starts and never registers is not healthy).
    pub registered_active: bool,
}

impl Health {
    pub fn is_healthy(self) -> bool {
        self.unit_active && self.ready && self.registered_active
    }

    /// What is not healthy, for a log line.
    pub fn problem(self) -> String {
        let mut missing = Vec::new();
        if !self.unit_active {
            missing.push("unit not active");
        }
        if !self.ready {
            missing.push("not ready");
        }
        if !self.registered_active {
            missing.push("not registered active at the commit");
        }
        missing.join(", ")
    }
}

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The host or the path to it did not answer (connection lost, timeout).
    #[error("{host}: unreachable: {why}")]
    Unreachable { host: HostName, why: String },
    /// The verb ran on the host and failed or was refused.
    #[error("{host}: {verb} failed: {why}")]
    Verb {
        host: HostName,
        verb: &'static str,
        why: String,
    },
}

/// The calls a rollout makes. See the module docs for the rules every implementation
/// keeps. Calls naming `frontend` are answered by the frontend host: the fleet lock,
/// the state-store backup and the cluster's replication count.
pub trait Transport {
    /// Take the fleet-wide rollout lock for `run`, or say who holds it. One rollout
    /// runs at a time; a second must find the lock held. A lock that outlives its
    /// holder's death is the implementation's to expire (the real one is tied to the
    /// connection that took it).
    fn lock(&mut self, frontend: &HostName, run: &RunId) -> Result<Lock, TransportError>;

    /// Release the lock `run` holds.
    fn unlock(&mut self, frontend: &HostName, run: &RunId) -> Result<(), TransportError>;

    /// A consistent copy of the frontend's SQLite state file, kept where the host's
    /// verbs put it (a restore point taken before anything changes).
    fn backup_state(&mut self, frontend: &HostName) -> Result<(), TransportError>;

    /// The blobs the cluster holds on fewer copies than it should, as the frontend
    /// counts them (`under_replicated_blobs`). 0 means every blob has all its copies.
    fn under_replicated_blobs(&mut self, frontend: &HostName) -> Result<u64, TransportError>;

    /// Put the files of `commit` on `host` and verify them there (checksum, the
    /// attestation, the units re-rendered for the new binary), without changing what
    /// runs. A refusal leaves the host as it was.
    fn stage(&mut self, host: &HostName, commit: &Commit) -> Result<(), TransportError>;

    /// Stop `host` taking new work: new leases for a daemon (a storage node's storage
    /// half keeps serving), new Execute calls for the frontend.
    fn drain(&mut self, host: &HostName) -> Result<(), TransportError>;

    /// The work a swap would break, still running on a drained `host`: leases on a
    /// daemon host, open Execute streams on the frontend. 0 means a swap breaks
    /// nothing.
    fn in_flight(&mut self, host: &HostName) -> Result<u64, TransportError>;

    /// Make the staged `commit` the running one, keeping the previous one for
    /// [`Transport::rollback`], and start the host's commit-confirm timer: if no
    /// [`Transport::confirm`] arrives within it, the host reverts by itself. The timer
    /// must outlive the rollout's health and repair waits
    /// ([`super::engine::Limits`]).
    fn swap(&mut self, host: &HostName, commit: &Commit) -> Result<(), TransportError>;

    /// One look at `host`'s health after a swap to `commit`.
    fn check(&mut self, host: &HostName, commit: &Commit) -> Result<Health, TransportError>;

    /// Cancel the commit-confirm timer: the host stays on the new commit.
    fn confirm(&mut self, host: &HostName) -> Result<(), TransportError>;

    /// Let `host` take work again.
    fn undrain(&mut self, host: &HostName) -> Result<(), TransportError>;

    /// Put the previous commit back through the same activation as a swap, and return
    /// when the host is healthy on it. The host stays drained.
    fn rollback(&mut self, host: &HostName) -> Result<(), TransportError>;

    /// Let `for_` pass between two polls. The real transport sleeps; the simulator
    /// moves its clock.
    fn sleep(&mut self, for_: Duration);
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
