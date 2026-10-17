//! Renewal (design section 5.8).
//!
//! The node renews when half the certificate's life has passed (the answer's
//! `renew_after`, spread by up to 10% so a fleet started together does not renew
//! together), retries every 5 minutes doubling to an hour on failure, and warns when less
//! than a day is left. The request is a CSR per role signed by **the key the registry
//! holds**, with the time it was made; there is no token and no client certificate, so a
//! node whose certificate expired during an outage recovers by itself, for as long as it
//! is not revoked.
//!
//! The decisions are functions of numbers (the clock is a parameter, the jitter is
//! drawn by the caller), so the schedule is tested without sleeping.

use kbf_proto::kbf::enroll::v1::{RenewRequest, RoleCsr};
use kbf_types::NodeId;

use crate::answer::{self, AnswerError, Expect};
use crate::client::{CallError, Frontend};
use crate::csr;
use crate::store::{Node, Store, StoreError, StoredConfig};

/// The first wait after a failed attempt, in seconds.
pub const RETRY_FIRST_S: i64 = 300;
/// The longest wait after failed attempts, in seconds.
pub const RETRY_MAX_S: i64 = 3600;
/// The largest jitter either way, in thousandths.
pub const JITTER_PERMILLE: i32 = 100;
/// Below this many seconds left, the node warns.
pub const WARN_BELOW_S: i64 = 24 * 3600;

/// The wait after consecutive failures: 300 s, doubling, capped at 3600 s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// No failures yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { failures: 0 }
    }

    /// Records a failure and returns how long to wait before the next attempt.
    pub fn failed(&mut self) -> i64 {
        let wait = RETRY_FIRST_S
            .saturating_mul(2i64.saturating_pow(self.failures))
            .min(RETRY_MAX_S);
        self.failures = self.failures.saturating_add(1);
        wait
    }

    /// Forgets the failures.
    pub fn succeeded(&mut self) {
        self.failures = 0;
    }
}

/// When to first try to renew: `renew_after` as the frontend gave it, with the wait from
/// `server_time` spread by `jitter_permille` (clamped to plus or minus 10%). Never before
/// `server_time`.
#[must_use]
pub fn first_attempt(server_time: i64, renew_after: i64, jitter_permille: i32) -> i64 {
    let jitter = i64::from(jitter_permille.clamp(-JITTER_PERMILLE, JITTER_PERMILLE));
    let wait = (renew_after - server_time).max(0);
    server_time + wait * (1000 + jitter) / 1000
}

/// Whether less than a day is left before the earliest certificate expires.
#[must_use]
pub fn near_expiry(node: &Node, now: i64) -> bool {
    node.earliest_not_after() - now < WARN_BELOW_S
}

/// Why a renewal failed.
#[derive(Debug, thiserror::Error)]
pub enum RenewError {
    /// The node has not enrolled.
    #[error("this node is not enrolled")]
    NotEnrolled,
    /// The frontend refused or could not be called. `RenewalExpired` and `UnknownNode`
    /// mean the node must enroll again with a token.
    #[error(transparent)]
    Call(#[from] CallError),
    /// The answer was refused; nothing was installed.
    #[error("the answer was refused: {0}")]
    Answer(#[from] AnswerError),
    /// A file could not be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Asks the frontend for new certificates for the keys the node holds, checks the answer
/// and installs it. The keys do not change, so the files replaced are the certificates
/// and the CA bundle, each in one step.
///
/// # Errors
/// [`RenewError`]; on `Call` and `Answer` nothing was installed.
pub async fn renew(frontend: &Frontend, store: &Store, now: i64) -> Result<Node, RenewError> {
    let node = store.recover()?.ok_or(RenewError::NotEnrolled)?;
    let node_id: NodeId = node
        .node_id
        .parse()
        .map_err(|_| StoreError::Damaged("node.json".to_owned()))?;
    let pin = node.pin()?;
    let mut keys = Vec::new();
    let mut csrs = Vec::new();
    for role in node.roles() {
        let key = store.key_pair(role)?;
        csrs.push(RoleCsr {
            role: role.name().to_owned(),
            csr_der: csr::request(&key, Some(now)).map_err(StoreError::from)?,
        });
        keys.push((role, csr::spki_of(&key)));
    }
    let response = frontend
        .renew(RenewRequest {
            node_id: node.node_id.clone(),
            csrs,
            issued_at: now,
            config_generation: node.config.generation,
        })
        .await?;
    let certs = answer::check(
        &Expect {
            pin: &pin,
            node_id: &node_id,
            keys: &keys,
            now,
        },
        &response.certs,
        &response.ca_bundle_pem,
        response.server_time,
        response.renew_after,
    )?;
    let config = response
        .config
        .as_ref()
        .map_or_else(|| node.config.clone(), StoredConfig::from_proto);
    let updated = Node::new(&node_id, &pin, &certs, response.renew_after, config);
    store.stage(&certs, &response.ca_bundle_pem, &updated)?;
    store.commit()?;
    Ok(updated)
}
