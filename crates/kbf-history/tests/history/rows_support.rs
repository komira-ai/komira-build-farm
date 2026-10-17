//! Helpers for the tests of the cross-build index rows: tests runs with a shape, and
//! the rows read back raw.

use kbf_history::records::{RunOutcome, RunRecord};
use kbf_history::{Event, InvKey, InvRef, Mark, ShapeRow, StatusKind, StatusRow};
use kbf_state::Store;
use prost::Message;

use crate::support::{Life, Objects, digest, failed_section, run_record};

/// The configuration every shape test run names unless it says otherwise.
pub const CFG: &str = "cfg-a";

pub fn key(name: &str) -> InvKey {
    InvKey::from_record_id(name)
}

/// Offers a run of `inv` at `t` that `edit` has changed, with its minted id.
pub fn offer_with(
    life: &Life,
    inv: &InvRef,
    t: u64,
    instance: &str,
    outcome: RunOutcome,
    edit: impl FnOnce(&mut RunRecord),
) -> RunRecord {
    let minted = life.minter.mint(&inv.key, t);
    let mut r = run_record(instance, outcome, t);
    r.set_run_id(minted.id);
    edit(&mut r);
    let failed = (outcome != RunOutcome::Ok).then(|| failed_section(instance));
    assert!(life.queue.offer(Event::Run {
        inv: inv.clone(),
        record: Box::new(r.clone()),
        failed,
        stream_opened_ms: minted.stream_opened_ms,
    }));
    r
}

/// A run of test shape `shape` with action digest `d`.
pub fn test_run(
    life: &Life,
    inv: &InvRef,
    t: u64,
    outcome: RunOutcome,
    shape: u64,
    d: u8,
) -> RunRecord {
    offer_with(life, inv, t, "main", outcome, |r| {
        r.shape = shape;
        r.cfg = CFG.into();
        r.test = true;
        r.action = Some(digest(d));
    })
}

/// A run of a shape that is not a test.
pub fn plain_run(life: &Life, inv: &InvRef, t: u64, shape: u64) -> RunRecord {
    offer_with(life, inv, t, "main", RunOutcome::Ok, |r| {
        r.shape = shape;
        r.cfg = CFG.into();
    })
}

pub fn status_key(life: &Life, instance: &str, shape: u64) -> String {
    life.index_root.status(instance, shape, CFG)
}

pub fn raw(store: &dyn Store, key: &str) -> Option<Vec<u8>> {
    store.get(key).unwrap().map(|v| v.value)
}

/// The status row of the test (`main`, `shape`).
pub fn status_row(life: &Life, shape: u64) -> Option<StatusRow> {
    let bytes = raw(life.index.inner.as_ref(), &status_key(life, "main", shape))?;
    Some(StatusRow::decode(bytes.as_slice()).unwrap())
}

pub fn shape_row(life: &Life, shape: u64) -> Option<ShapeRow> {
    let key = life.index_root.shape("main", shape, CFG);
    let bytes = raw(life.index.inner.as_ref(), &key)?;
    Some(ShapeRow::decode(bytes.as_slice()).unwrap())
}

/// The by-status rows of `kind`, keys only.
pub fn statusx(life: &Life, kind: StatusKind) -> Vec<String> {
    life.index
        .keys(&life.index_root.statusx_prefix(kind.name()))
}

/// Every by-status row, keys only.
pub fn all_statusx(life: &Life) -> Vec<String> {
    life.index
        .keys(&format!("{}statusx/", life.index_root.as_str()))
}

pub fn run_ids(row: &StatusRow) -> Vec<Vec<u8>> {
    row.ring.iter().map(|r| r.run_id.clone()).collect()
}

pub fn id(r: &RunRecord) -> Vec<u8> {
    r.run_id().to_bytes().to_vec()
}

pub fn mark_run(m: &Option<Mark>) -> Option<Vec<u8>> {
    m.as_ref().map(|m| m.run_id.clone())
}

/// A second life over the same stores: another frontend, or the same one restarted.
pub fn restart(life: &Life, writer: u64) -> Life {
    let objects: &Objects = &life.objects;
    Life::start(objects, &life.index, writer, kbf_history::Limits::default())
}
