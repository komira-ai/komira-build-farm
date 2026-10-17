//! Index writes not yet applied, and the objects whose rows must be put back.
//!
//! [`Pending`] holds `ui/` row writes in memory, coalesced per key (a second write to a
//! row replaces the first and moves to the back), within a key and a byte bound. Rows
//! are taken in the order they came, a chunk's rows together, so a chunk's pointer
//! rows and the header written with them share one transaction. Past either bound, a new row is
//! refused and the caller counts it under `stage="index"`; nothing waits. No record is
//! lost by that: every pointer row names an object already written, and the writer
//! keeps that object's key in [`Kept`] and restores its rows from the object's footer
//! once the backlog drains, with no LIST.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kbf_objstore::ObjectKey;
use kbf_state::{Cond, Txn};

/// An object whose index rows were dropped: its key and footer range (about 100 bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptObject {
    /// The object.
    pub key: ObjectKey,
    /// Where its footer starts.
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    pub footer_len: u32,
}

/// A write that commits with a row, in the same transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Also {
    /// Store `value` under the key, whatever is there.
    Put(String, Vec<u8>),
    /// Delete the key, whatever is there.
    Delete(String),
}

impl Also {
    fn bytes(&self) -> usize {
        match self {
            Also::Put(key, value) => key.len() + value.len(),
            Also::Delete(key) => key.len(),
        }
    }
}

/// A row write waiting to be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The value.
    pub value: Vec<u8>,
    /// Its precondition: [`Cond::Absent`] for a pointer row (a row that exists already
    /// is the same row), [`Cond::Any`] for a header.
    pub cond: Cond,
    /// The object the row was made from, to keep if the row is dropped.
    pub origin: Option<KeptObject>,
    /// Writes that apply with the row or not at all (a status row's by-status entry
    /// moves with it).
    pub also: Vec<Also>,
}

impl Row {
    /// A row with no companion writes.
    #[must_use]
    pub fn new(value: Vec<u8>, cond: Cond, origin: Option<KeptObject>) -> Row {
        Row {
            value,
            cond,
            origin,
            also: Vec::new(),
        }
    }

    fn bytes(key: &str, row: &Row) -> usize {
        key.len() + row.value.len() + row.also.iter().map(Also::bytes).sum::<usize>()
    }

    /// The transaction of `batch`, and for each of its operations the index of the row
    /// of `batch` it belongs to (a conflict names an operation).
    pub fn txn(batch: &[(String, Row)]) -> (Txn, Vec<usize>) {
        let mut txn = Txn::new();
        let mut owner = Vec::with_capacity(batch.len());
        for (at, (key, row)) in batch.iter().enumerate() {
            txn = txn.put(key.clone(), row.value.clone(), row.cond);
            owner.push(at);
            for also in &row.also {
                txn = match also {
                    Also::Put(key, value) => txn.put(key.clone(), value.clone(), Cond::Any),
                    Also::Delete(key) => txn.delete(key.clone(), Cond::Any),
                };
                owner.push(at);
            }
        }
        (txn, owner)
    }
}

/// The bounds of [`Pending`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingLimits {
    /// Distinct keys held at most.
    pub keys: usize,
    /// Key and value bytes held at most.
    pub bytes: usize,
}

impl Default for PendingLimits {
    /// 131,072 keys or 32 MiB.
    fn default() -> PendingLimits {
        PendingLimits {
            keys: 131_072,
            bytes: 32 * 1024 * 1024,
        }
    }
}

/// Row writes not yet applied, coalesced per key, in the order they came.
#[derive(Debug)]
pub struct Pending {
    /// The rows, by when each last came.
    order: BTreeMap<u64, (String, Row)>,
    /// Each pending key's place in `order`.
    at: BTreeMap<String, u64>,
    next: u64,
    bytes: usize,
    limits: PendingLimits,
}

impl Pending {
    /// Nothing pending.
    #[must_use]
    pub fn new(limits: PendingLimits) -> Pending {
        Pending {
            order: BTreeMap::new(),
            at: BTreeMap::new(),
            next: 0,
            bytes: 0,
            limits,
        }
    }

    /// Adds a write of `key` at the back, replacing a pending write of the same key. A
    /// new key past either bound is refused and the row given back.
    ///
    /// # Errors
    /// The row, when it was not taken.
    pub fn insert(&mut self, key: String, row: Row) -> Result<(), Row> {
        let size = Row::bytes(&key, &row);
        if let Some(old) = self.at.get(&key) {
            let old = self.order.remove(old);
            self.bytes -= old.map_or(0, |(k, r)| Row::bytes(&k, &r));
        } else if self.at.len() >= self.limits.keys || self.bytes + size > self.limits.bytes {
            return Err(row);
        }
        self.bytes += size;
        self.at.insert(key.clone(), self.next);
        self.order.insert(self.next, (key, row));
        self.next += 1;
        Ok(())
    }

    /// The front run of rows made from one object (a row with no object is a run of
    /// one): how many, and their bytes.
    fn front_group(&self) -> (usize, usize) {
        let mut rows = self.order.values();
        let Some((key, first)) = rows.next() else {
            return (0, 0);
        };
        let mut n = 1;
        let mut size = Row::bytes(key, first);
        for (key, row) in rows {
            if first.origin.is_none() || row.origin != first.origin {
                break;
            }
            n += 1;
            size += Row::bytes(key, row);
        }
        (n, size)
    }

    fn pop_front(&mut self) -> Option<(String, Row)> {
        let (_, (key, row)) = self.order.pop_first()?;
        self.at.remove(&key);
        self.bytes -= Row::bytes(&key, &row);
        Some((key, row))
    }

    /// Takes up to `max_rows` writes whose bytes stay within `max_bytes`, oldest first
    /// and the rows of one object whole: a group that does not fit waits for the next
    /// take, unless nothing was taken yet, when as much of it as fits is taken (always
    /// at least one row, so one oversized row cannot wedge the queue).
    pub fn take(&mut self, max_rows: usize, max_bytes: usize) -> Vec<(String, Row)> {
        let mut out = Vec::new();
        let mut size = 0;
        loop {
            let (n, group) = self.front_group();
            let fits = out.len() + n <= max_rows && size + group <= max_bytes;
            if n == 0 || !fits {
                break;
            }
            out.extend((0..n).filter_map(|_| self.pop_front()));
            size += group;
        }
        if !out.is_empty() {
            return out;
        }
        // The front group alone is past a bound: as much of it as fits, at least a row.
        while out.len() < max_rows {
            let Some((key, row)) = self.order.values().next() else {
                break;
            };
            let n = Row::bytes(key, row);
            if !out.is_empty() && size + n > max_bytes {
                break;
            }
            size += n;
            out.extend(self.pop_front());
        }
        out
    }

    /// Whether a write of `key` is pending.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.at.contains_key(key)
    }

    /// Writes pending.
    #[must_use]
    pub fn len(&self) -> usize {
        self.at.len()
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

/// Objects whose rows must be restored, oldest first, each once. Bounded: past the
/// bound the oldest falls off, to the weekly sweep that adopts orphans by LIST.
#[derive(Debug)]
pub struct Kept {
    order: VecDeque<KeptObject>,
    keys: BTreeSet<ObjectKey>,
    max: usize,
}

impl Kept {
    /// An empty list holding at most `max` objects.
    #[must_use]
    pub fn new(max: usize) -> Kept {
        Kept {
            order: VecDeque::new(),
            keys: BTreeSet::new(),
            max,
        }
    }

    /// Keeps `object` unless it is kept already. Returns the object that fell off the
    /// front to make room, if one did.
    pub fn push(&mut self, object: KeptObject) -> Option<KeptObject> {
        if self.max == 0 || !self.keys.insert(object.key.clone()) {
            return None;
        }
        let fell = if self.order.len() >= self.max {
            self.order.pop_front()
        } else {
            None
        };
        if let Some(f) = &fell {
            self.keys.remove(&f.key);
        }
        self.order.push_back(object);
        fell
    }

    /// Takes the oldest kept object.
    pub fn pop(&mut self) -> Option<KeptObject> {
        let object = self.order.pop_front()?;
        self.keys.remove(&object.key);
        Some(object)
    }

    /// Objects kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: &str) -> Row {
        Row::new(value.as_bytes().to_vec(), Cond::Absent, None)
    }

    fn kept(name: &str) -> KeptObject {
        KeptObject {
            key: ObjectKey::new(format!("history/{name}")).unwrap(),
            footer_offset: 1,
            footer_len: 2,
        }
    }

    #[test]
    fn writes_coalesce_per_key_and_new_keys_stop_at_the_bounds() {
        // Catches: a second write of a row taking a key of its own (the bound would
        // fill with stale versions), or a bound that refuses a replacement.
        let mut p = Pending::new(PendingLimits {
            keys: 2,
            bytes: 100,
        });
        assert!(p.is_empty());
        p.insert("a".into(), row("1")).unwrap();
        p.insert("a".into(), row("22")).unwrap();
        assert_eq!(p.len(), 1);
        p.insert("b".into(), row("3")).unwrap();
        assert_eq!(p.insert("c".into(), row("4")), Err(row("4")));
        p.insert("b".into(), row("5")).unwrap();
        let taken = p.take(10, 1000);
        assert_eq!(taken, [("a".into(), row("22")), ("b".into(), row("5"))]);
        assert!(p.is_empty());
        let mut small = Pending::new(PendingLimits { keys: 10, bytes: 5 });
        small.insert("k".into(), row("1234")).unwrap();
        assert!(
            small.insert("j".into(), row("1")).is_err(),
            "the byte bound holds"
        );
        assert_eq!(small.take(1, 0).len(), 1);
        small.insert("j".into(), row("1")).unwrap();
    }

    #[test]
    fn take_bounds_rows_and_bytes_but_always_takes_one() {
        // Catches: a batch over the background transaction's row or byte cap, or an
        // oversized row that is never taken (the queue would wedge behind it).
        let mut p = Pending::new(PendingLimits::default());
        for k in ["a", "b", "c"] {
            p.insert(k.into(), row("xxxx")).unwrap();
        }
        assert_eq!(p.take(2, 1000).len(), 2);
        p.insert("a".into(), row("xxxx")).unwrap();
        let big: Vec<_> = p.take(10, 1).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            big,
            ["c"],
            "oldest first, and one though it is past the bytes"
        );
        assert_eq!(p.take(10, 1000).len(), 1);
        assert!(p.take(10, 1000).is_empty());
        assert!(p.take(0, 1000).is_empty());
    }

    fn from(value: &str, object: &str) -> Row {
        Row {
            origin: Some(kept(object)),
            ..row(value)
        }
    }

    fn keys(taken: Vec<(String, Row)>) -> Vec<String> {
        taken.into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn rows_come_out_in_arrival_order_with_one_object_s_rows_together() {
        // Catches: rows taken in key order, which puts every `ui/ichunk` row ahead of
        // every `ui/invid` row, so under a backlog a chunk row lands transactions before
        // the header written with it (a reader then sees a chunk naming instances the
        // header does not list); a batch cut through one object's rows; and a rewritten
        // header left at its old place, ahead of the chunk rows it was written with.
        let mut p = Pending::new(PendingLimits::default());
        p.insert("ui/stream/a".into(), from("s", "a")).unwrap();
        p.insert("ui/ichunk/a".into(), from("i", "a")).unwrap();
        p.insert("ui/invid/x".into(), from("h", "a")).unwrap();
        p.insert("ui/ichunk/b".into(), from("i", "b")).unwrap();
        p.insert("ui/invid/x".into(), from("h2", "b")).unwrap();
        p.insert("ui/invid/y".into(), row("h")).unwrap();
        p.insert("ui/invid/z".into(), row("h")).unwrap();
        assert_eq!(
            keys(p.take(3, 1000)),
            ["ui/stream/a", "ui/ichunk/a"],
            "chunk b's two rows wait rather than split"
        );
        assert_eq!(keys(p.take(2, 1000)), ["ui/ichunk/b", "ui/invid/x"]);
        assert_eq!(
            keys(p.take(1, 1000)),
            ["ui/invid/y"],
            "rows of no object stand alone"
        );
        p.insert("ui/stream/c".into(), from("s", "c")).unwrap();
        p.insert("ui/ichunk/c".into(), from("i", "c")).unwrap();
        p.insert("ui/invid/c".into(), from("h", "c")).unwrap();
        assert_eq!(
            keys(p.take(10, 1000)),
            ["ui/invid/z", "ui/stream/c", "ui/ichunk/c", "ui/invid/c"]
        );
        p.insert("ui/stream/d".into(), from("s", "d")).unwrap();
        p.insert("ui/ichunk/d".into(), from("i", "d")).unwrap();
        p.insert("ui/invid/d".into(), from("h", "d")).unwrap();
        assert_eq!(
            keys(p.take(1, 1000)),
            ["ui/stream/d"],
            "a group past the row bound alone is split"
        );
        assert_eq!(
            keys(p.take(10, 13)),
            ["ui/ichunk/d"],
            "and one past the byte bound"
        );
        assert_eq!(keys(p.take(10, 1000)), ["ui/invid/d"]);
        for (key, value) in [
            ("ui/stream/f", "s"),
            ("ui/ichunk/f", "i"),
            ("ui/invid/f", "h"),
        ] {
            p.insert(key.into(), from(value, "f")).unwrap();
        }
        assert_eq!(
            keys(p.take(2, 1000)),
            ["ui/stream/f", "ui/ichunk/f"],
            "a split takes as many rows as the bound allows"
        );
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn kept_objects_are_unique_bounded_and_oldest_first() {
        // Catches: one object kept once per dropped row (re-read three times), an
        // unbounded list, or the newest falling off instead of the oldest.
        let mut k = Kept::new(2);
        assert_eq!(k.len(), 0);
        assert_eq!(k.push(kept("a")), None);
        assert_eq!(k.push(kept("a")), None);
        assert_eq!(k.len(), 1);
        assert_eq!(k.push(kept("b")), None);
        assert_eq!(k.push(kept("c")), Some(kept("a")));
        assert_eq!(
            k.push(kept("a")),
            Some(kept("b")),
            "a fell off, so it is new again"
        );
        assert_eq!(k.pop(), Some(kept("c")));
        assert_eq!(k.pop(), Some(kept("a")));
        assert_eq!(k.pop(), None);
        let mut none = Kept::new(0);
        assert_eq!(none.push(kept("a")), None);
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn a_row_s_companions_share_its_transaction_and_its_bytes() {
        // Catches: a companion write that leaves the row's transaction (a status row
        // would land without its by-status entry), a conflict on a companion blamed on
        // the wrong row, and a byte bound that ignores the companions.
        let mut with = Row::new(b"v".to_vec(), Cond::Version(kbf_state::Version(3)), None);
        with.also = vec![
            Also::Delete("d".into()),
            Also::Put("p".into(), b"xyz".to_vec()),
        ];
        let batch = vec![("a".to_owned(), row("1")), ("b".to_owned(), with.clone())];
        let (txn, owner) = Row::txn(&batch);
        assert_eq!(owner, [0, 1, 1, 1]);
        let expect = Txn::new()
            .put("a", b"1".to_vec(), Cond::Absent)
            .put("b", b"v".to_vec(), Cond::Version(kbf_state::Version(3)))
            .delete("d", Cond::Any)
            .put("p", b"xyz".to_vec(), Cond::Any);
        assert_eq!(txn, expect);
        assert_eq!(Row::bytes("b", &with), 1 + 1 + 1 + 1 + 3);
        let mut p = Pending::new(PendingLimits { keys: 10, bytes: 6 });
        assert!(p.insert("b".into(), with).is_err(), "7 bytes do not fit 6");
    }
}
