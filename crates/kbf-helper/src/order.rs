//! An order file: what the unprivileged `kbf` leaves in `inbox/orders/` after the
//! frontend ordered this host to activate, confirm or revert a release.
//!
//! `<order_id:020>.json`, written by the node's update client (`kbf-update`) as
//! `{"order_id":N,"epoch":E,"verb":"activate","commit":"<40 hex>","window_s":S}`. The
//! writer is the service user, so the file is untrusted: the name must be the id, the
//! verb one of three words, the commit a full commit, and no other field is accepted.

use kbf_installer::CommitId;
use serde::Deserialize;

/// The suffix of an order file.
pub const SUFFIX: &str = ".json";

/// How far above the last acted-on epoch an order's may be. Epochs move only when a store is
/// restored (a handover keeps it), so a jump of more than this is a mistake or an attack;
/// either would put every real order below it. Such an order is refused, not acknowledged.
/// Not applied to the first order a host acts on (it has no epoch to measure from).
pub const MAX_EPOCH_JUMP: u64 = 1000;

/// The biggest order file read.
pub const MAX_BYTES: u64 = 4096;

/// What an order asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Activate,
    Confirm,
    Revert,
}

/// A checked order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: u64,
    /// The `Welcome` epoch of the session the node received it in.
    pub epoch: u64,
    pub verb: Verb,
    pub commit: CommitId,
    /// The confirm window the frontend asks for; the helper clamps it.
    pub window_s: u32,
}

/// Why a file is not an order.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OrderFault {
    #[error("not an order: {0}")]
    NotAnOrder(String),
    #[error("unknown verb {0:?}")]
    UnknownVerb(String),
    #[error("{0:?} is not a commit (40 lowercase hex characters)")]
    BadCommit(String),
    #[error("the file is named for order {file} but holds order {inside}")]
    NameIsNotTheId { file: u64, inside: u64 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    order_id: u64,
    epoch: u64,
    verb: String,
    commit: String,
    window_s: u32,
}

/// The id an order file's name stands for: twenty digits and [`SUFFIX`].
pub fn id_of(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    if (digits.len() != 20) | !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The order in `bytes`, which was read from the file for order `file_id`.
pub fn parse(file_id: u64, bytes: &[u8]) -> Result<Order, OrderFault> {
    let raw: Raw =
        serde_json::from_slice(bytes).map_err(|e| OrderFault::NotAnOrder(e.to_string()))?;
    if raw.order_id != file_id {
        return Err(OrderFault::NameIsNotTheId {
            file: file_id,
            inside: raw.order_id,
        });
    }
    let verb = match raw.verb.as_str() {
        "activate" => Verb::Activate,
        "confirm" => Verb::Confirm,
        "revert" => Verb::Revert,
        other => return Err(OrderFault::UnknownVerb(other.to_owned())),
    };
    let commit = CommitId::new(&raw.commit).map_err(|_| OrderFault::BadCommit(raw.commit))?;
    Ok(Order {
        id: raw.order_id,
        epoch: raw.epoch,
        verb,
        commit,
        window_s: raw.window_s,
    })
}
