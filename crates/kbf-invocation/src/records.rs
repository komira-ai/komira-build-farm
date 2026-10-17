//! Every record, built from the event stream: which record a call goes to (section
//! 2.4), which rows an operation's lease events reach (section 3.2), and the memory
//! bound of section 3.5, stage 1.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kbf_types::{Digest, LeaseId, OperationId};

use crate::event::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Listing, Outcome,
    Owner, WallTime,
};
use crate::record::{
    ActionRow, Attempt, AttemptEnd, How, Invocation, OUTPUTS_PER_ROW, REASON_CAP, ROW_CAP,
    RecordKey, State, TEXT_CAP, attempt_bytes, attempts_bytes, btree_entry_bytes, capped,
    output_bytes, run_bytes, shape_bytes,
};
use crate::run::ActionShape;

mod ended;

pub use ended::{Ended, EndedKind};

/// How many dropped calls of records not made yet [`Records`] remembers, so that the
/// record each would have made starts incomplete (see
/// [`Records::mark_incomplete_call`]).
const UNBORN_CAP: usize = 1024;

/// The share of the bound the dropped calls of records not made yet may hold besides
/// it: one part in this many.
const UNBORN_SHARE: usize = 16;

/// What an operation's entry takes beside its attempts and waiters: its key and value's
/// share of the operation map's nodes.
const OPERATION_BYTES: usize = btree_entry_bytes(size_of::<OperationId>() + size_of::<Operation>());

const HELD: &str = "every key Records hands out names a record it holds";

/// Every invocation record, and the operations their Execute calls wait on.
///
/// [`Records::apply`] is the way in: the records are a function of the events applied,
/// in order, and of the drops marked between them. Nothing here reads a clock or does
/// I/O: "now" is the latest time an event carried.
///
/// **The memory bound** (section 3.5, stage 1): the records' [`Invocation::bytes`]
/// and the operations their calls wait on ([`Records::operations_bytes`]) together
/// stay within the bound given to [`Records::with_bound`], after every event and
/// every mark. Whatever an event adds is admitted first: a record, a row, a header
/// field, a waiting call, an attempt, an outcome's text, a listing. When it does not
/// fit, records are removed in this order:
///
/// 1. minted one-action records (section 2.3), least recently seen first;
/// 2. idle records ([`State::Idle`]), least recently seen first. A record with an
///    operation in flight is never idle, however long that operation runs;
/// 3. if only open records remain, nothing is removed: the largest open record stops
///    taking rows (its actions are still counted, and it says `rows_capped`). What
///    did not fit is left out: a row is not taken (its record says `rows_capped`); a
///    listing keeps the outputs that fit (the row says `outputs_truncated`); a record
///    is not made (its caller's next record starts incomplete, as for a dropped
///    call); anything else (an attempt, an outcome's text, a waiting call, an
///    instance name, a later Execute's row) is not set. In every case but a row's,
///    the record says `complete: false`. A call whose instance name was left out
///    is left out whole (no counter, `last_seen`, row or waiting call): a reader
///    is authorized on the record's names, so a row never holds another.
///
/// **What a byte is**: what the allocator is asked for. The count follows the
/// types' sizes, the row lists' capacity, every copy of an id, and B-tree nodes as
/// estimated from the standard library's layout (`btree_bytes`); every other list
/// is allocated to its length. Not counted: the first node of each index (about
/// 2 kB together), the allocator's own overhead per allocation, and the dropped
/// calls of records not made yet, which are held to a sixteenth of the bound
/// besides it (at most `UNBORN_CAP`, 1,024; past either the oldest is forgotten,
/// and one that does not fit alone is not kept). Measured with a counting
/// allocator (`tests/heap.rs`), the records' heap reached at most 1.08 times the
/// bound at 200 kB and above, and 1.12 at 50 kB; the tests fail past 1.25.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    row_cap: usize,
    /// The memory bound, in bytes.
    bound: usize,
    /// The records' [`Invocation::bytes`] and the operations' bytes together, as last
    /// counted.
    bytes: usize,
    /// The latest time an event carried.
    now: WallTime,
    /// Boxed: a B-tree node has room for 11 values, and many are unused.
    invocations: BTreeMap<RecordKey, Box<Invocation>>,
    /// Each id's record keys: the owner's first, then each collision in the order seen.
    ids: BTreeMap<InvocationId, Family>,
    operations: BTreeMap<OperationId, Operation>,
    /// Every record by when it was last seen, oldest first.
    by_seen: BTreeSet<(WallTime, RecordKey)>,
    /// The minted records by when they were last seen, oldest first.
    minted_by_seen: BTreeSet<(WallTime, RecordKey)>,
    /// Callers whose dropped call named no record yet: the record their next call
    /// makes starts incomplete. The oldest is forgotten past [`UNBORN_CAP`].
    unborn: VecDeque<(InvocationId, Owner)>,
    /// What `unborn`'s ids and owners hold.
    unborn_bytes: usize,
    /// Records made so far: the next one's [`Invocation::serial`].
    made: u64,
    /// The latest `last_seen` of a client's record removed for the bound.
    evicted_seen: Option<WallTime>,
    /// Records removed for the bound.
    evicted: u64,
    /// What the event being applied ended; [`Records::apply`] hands it out.
    ended: Vec<Ended>,
}

/// The records of one id that are held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Family {
    /// Their keys, in the order made.
    keys: Vec<RecordKey>,
    /// The `n` of the next collision key `<id>~<n>`. It only grows while a record of
    /// the id is held, so no two records held at once get one key.
    next: usize,
}

/// An operation some record waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Operation {
    /// When it was last queued: submitted, or requeued.
    queued: WallTime,
    /// Its leases, in grant order.
    attempts: Vec<Attempt>,
    /// The calls waiting on it: each one's record, its row (`None` past the row cap)
    /// and the REAPI operation name it was given.
    waiters: Vec<Waiter>,
    /// What [`Records`] counts for it toward the bound.
    counted: usize,
}

impl Operation {
    /// The memory it holds, as the bound counts it.
    fn bytes(&self) -> usize {
        OPERATION_BYTES
            + attempts_bytes(&self.attempts)
            + self.waiters.iter().map(Waiter::bytes).sum::<usize>()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Waiter {
    key: RecordKey,
    row: Option<usize>,
    name: String,
}

impl Waiter {
    /// The index of the waiter's row in `inv`, while the row still shows this call: a
    /// later Execute of the same action replaces the row, and this operation's events
    /// no longer reach it.
    fn row(&self, inv: &Invocation) -> Option<usize> {
        let i = self.row?;
        (inv.rows[i].operation.as_deref() == Some(self.name.as_str())).then_some(i)
    }

    /// What it takes in its operation.
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.key.id().as_str().len() + self.name.len()
    }
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    /// No records, each to hold up to [`ROW_CAP`] rows, with no memory bound.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_row_cap(ROW_CAP)
    }

    /// No records, each to hold up to `row_cap` rows, with no memory bound.
    #[must_use]
    pub const fn with_row_cap(row_cap: usize) -> Self {
        Self::with_bound(row_cap, usize::MAX)
    }

    /// No records, each to hold up to `row_cap` rows, all of them together within
    /// `bound` bytes (see the type's docs).
    #[must_use]
    pub const fn with_bound(row_cap: usize, bound: usize) -> Self {
        Self {
            row_cap,
            bound,
            bytes: 0,
            now: WallTime::from_unix_micros(0),
            invocations: BTreeMap::new(),
            ids: BTreeMap::new(),
            operations: BTreeMap::new(),
            by_seen: BTreeSet::new(),
            minted_by_seen: BTreeSet::new(),
            unborn: VecDeque::new(),
            unborn_bytes: 0,
            evicted: 0,
            made: 0,
            evicted_seen: None,
            ended: Vec::new(),
        }
    }

    /// The record under `key`.
    #[must_use]
    pub fn get(&self, key: &RecordKey) -> Option<&Invocation> {
        self.invocations.get(key).map(|inv| &**inv)
    }

    /// The record under `key`, to mark it incomplete.
    pub fn get_mut(&mut self, key: &RecordKey) -> Option<&mut Invocation> {
        self.invocations.get_mut(key).map(|inv| &mut **inv)
    }

    /// Every record's key, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &RecordKey> {
        self.invocations.keys()
    }

    /// What the bound counts: the records' [`Invocation::bytes`] and
    /// [`Self::operations_bytes`] together.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// What the operations the records' calls wait on hold, counted afresh: each
    /// entry, its attempts and its waiting calls.
    #[must_use]
    pub fn operations_bytes(&self) -> usize {
        self.operations.values().map(Operation::bytes).sum()
    }

    /// The memory bound, in bytes.
    #[must_use]
    pub const fn bound(&self) -> usize {
        self.bound
    }

    /// How many records were removed for the bound.
    #[must_use]
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// The latest `last_seen` of a record of a client's id removed for the bound, or
    /// `None` if none was: every record removed was last seen at or before it, so a
    /// build seen after it would still be held. Minted records (section 2.3) do not
    /// count; their ids are the server's own (`docs/design/invocations-api.md`, 4.10).
    #[must_use]
    pub const fn evicted_last_seen(&self) -> Option<WallTime> {
        self.evicted_seen
    }

    /// Applies one event, admitting what it adds within the bound, and returns what it
    /// ended: each attempt of each row that settled, and each cache hit that gained
    /// its row (see [`Ended`]).
    pub fn apply(&mut self, event: Event) -> Vec<Ended> {
        self.apply_event(event);
        std::mem::take(&mut self.ended)
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::Call(call) => self.call(&call),
            Event::Command {
                id,
                owner,
                action,
                command,
                shape,
            } => {
                if let Some(key) = self.find(&id, &owner)
                    && let Some(i) = self.record(&key).row_of(&action)
                {
                    // A digest, held in the row: nothing to admit.
                    self.record(&key).rows[i].command = Some(command);
                    if let Some(shape) = shape {
                        self.fill_shape(&key, i, shape);
                    }
                }
            }
            Event::Granted {
                operation,
                lease,
                worker,
                at,
            } => {
                if let Some(op) = self.operations.get(&operation) {
                    let attempt = Attempt {
                        lease,
                        worker,
                        queued: op.queued,
                        granted: at,
                        started: None,
                        ended: None,
                        end: None,
                        run: None,
                        run_id: None,
                    };
                    let fits = self.make_room(None, attempt_bytes(&attempt));
                    // Making room may have removed every record waiting on it.
                    if let Some(op) = self.operations.get_mut(&operation) {
                        if fits {
                            // Allocated to its length: the bound counts lengths.
                            op.attempts.reserve_exact(1);
                            op.attempts.push(attempt);
                            self.recount_operation(operation);
                        } else {
                            self.mark_incomplete_operation(operation);
                        }
                        self.spread(operation, at);
                    }
                }
            }
            Event::Started {
                operation,
                lease,
                at,
            } => {
                if let Some(op) = self.operations.get_mut(&operation) {
                    if let Some(a) = lease_of(&mut op.attempts, lease) {
                        a.started = Some(at);
                    }
                    self.spread(operation, at);
                }
            }
            Event::Requeued {
                operation,
                lease,
                reason,
                at,
            } => {
                if let Some(op) = self.operations.get_mut(&operation) {
                    if let Some(a) = lease_of(&mut op.attempts, lease) {
                        a.ended = Some(at);
                        a.end = Some(AttemptEnd::Requeued(reason));
                    }
                    op.queued = at;
                    self.spread(operation, at);
                    // A report that carries a run block follows (`Ran`), which then
                    // settles the attempt.
                    if !ended::reports(reason) {
                        self.settled(operation, lease, EndedKind::Requeued, at);
                    }
                }
            }
            Event::Ran {
                operation,
                lease,
                run,
            } => {
                let held = self.operations.get(&operation);
                let Some(i) = held.and_then(|op| op.attempts.iter().position(|a| a.lease == lease))
                else {
                    return;
                };
                let fits = self.make_room(None, run_bytes(&run));
                // Making room may have removed every record waiting on it; it leaves
                // the attempts of an operation it keeps as they were.
                if let Some(op) = self.operations.get_mut(&operation) {
                    if fits {
                        op.attempts[i].run = Some(run);
                        self.recount_operation(operation);
                    } else {
                        self.mark_incomplete_operation(operation);
                    }
                    let at = self.now;
                    self.spread(operation, at);
                    self.settled(operation, lease, EndedKind::Ran, at);
                }
            }
            Event::Finished(finished) => self.finish(finished),
        }
    }

    /// Sets `shape` on row `i` of `key`'s record, its target's label capped as a
    /// `RequestMetadata` text is; left out if it does not fit (the record then
    /// marked incomplete).
    fn fill_shape(&mut self, key: &RecordKey, i: usize, mut shape: Box<ActionShape>) {
        if let Some(target) = &mut shape.target {
            let value = capped(&target.label.value, TEXT_CAP);
            target.label.truncated |= value.len() < target.label.value.len();
            target.label.value = value;
        }
        let had = self.record(key).rows[i]
            .shape
            .as_deref()
            .map_or(0, shape_bytes);
        if self.make_room(Some(key), shape_bytes(&shape).saturating_sub(had)) {
            let inv = self.record(key);
            inv.rows[i].shape = Some(shape);
            inv.refresh_row(i);
        } else {
            self.record(key).mark_incomplete();
        }
        self.recount(key);
    }

    /// Marks incomplete the record a dropped call of `id` by `owner` went to: the
    /// one held, or else the one that caller's next call makes.
    pub fn mark_incomplete_call(&mut self, id: &InvocationId, owner: &Owner) {
        match self.find(id, owner) {
            Some(key) => self.record(&key).mark_incomplete(),
            None => {
                let held = id.as_str().len() + owner.0.len();
                // The oldest is forgotten until the new one fits; one that does not fit
                // alone is not kept.
                loop {
                    let slots = self.unborn.capacity().max(self.unborn.len() + 1);
                    let bytes = slots * size_of::<(InvocationId, Owner)>() + self.unborn_bytes;
                    if self.unborn.len() < UNBORN_CAP && bytes + held <= self.bound / UNBORN_SHARE {
                        break;
                    }
                    let Some((id, owner)) = self.unborn.pop_front() else {
                        return;
                    };
                    self.unborn_bytes -= id.as_str().len() + owner.0.len();
                }
                // Allocated to its length, so that its slots stay within the share.
                self.unborn.reserve_exact(1);
                self.unborn.push_back((id.clone(), owner.clone()));
                self.unborn_bytes += held;
            }
        }
    }

    /// Marks incomplete every record that waits on `operation`, whose event was
    /// dropped.
    pub fn mark_incomplete_operation(&mut self, operation: OperationId) {
        let keys: Vec<RecordKey> = self
            .operations
            .get(&operation)
            .map(|op| op.waiters.iter().map(|w| w.key.clone()).collect())
            .unwrap_or_default();
        for key in keys {
            self.record(&key).mark_incomplete();
        }
    }

    /// Marks every record incomplete: an event was dropped and which records it
    /// concerned is not known.
    pub fn mark_all_incomplete(&mut self) {
        for inv in self.invocations.values_mut() {
            inv.mark_incomplete();
        }
    }

    /// The record under `key`, which this holds: every key in `ids`, in `by_seen`
    /// and in an operation's waiters names a record held.
    fn record(&mut self, key: &RecordKey) -> &mut Invocation {
        self.invocations.get_mut(key).expect(HELD)
    }

    /// The key of the record of `id` whose owner is `owner`, if there is one.
    fn find(&self, id: &InvocationId, owner: &Owner) -> Option<RecordKey> {
        self.ids
            .get(id)?
            .keys
            .iter()
            .find(|k| self.invocations[*k].owner == *owner)
            .cloned()
    }

    /// The key of the record `call` goes to, made if it is the first call of its id
    /// from its caller. With a record of the id held, a caller other than its owners
    /// gets `<id>~<n>`, and the owner's record (if held) lists it; with none held, the
    /// caller gets the id itself. `None` if a record to make does not fit: the call
    /// is left out, as a dropped one is.
    fn route(&mut self, call: &Call) -> Option<RecordKey> {
        if let Some(key) = self.find(&call.id, &call.owner) {
            return Some(key);
        }
        let mut inv = Invocation::new(RecordKey::owner(call.id.clone()), call);
        inv.seen(call);
        // A collision key holds no more than the owner's; the owner's list grows by it.
        let listed = size_of::<RecordKey>() + call.id.as_str().len();
        if !self.make_room(None, inv.bytes() + listed) {
            self.mark_incomplete_call(&call.id, &call.owner);
            return None;
        }
        let family = self.ids.entry(call.id.clone()).or_default();
        let key = if family.keys.is_empty() {
            family.next = 2;
            RecordKey::owner(call.id.clone())
        } else {
            family.next += 1;
            RecordKey::collision(call.id.clone(), family.next - 1)
        };
        // Every list here is allocated to its length: the bound counts lengths.
        family.keys.reserve_exact(1);
        family.keys.push(key.clone());
        let owner = RecordKey::owner(call.id.clone());
        if key != owner
            && let Some(inv) = self.invocations.get_mut(&owner)
        {
            inv.collisions.reserve_exact(1);
            inv.collisions.push(key.clone());
            self.recount(&owner);
        }
        inv.key = key.clone();
        inv.serial = self.made;
        self.made += 1;
        if let Some(i) = self
            .unborn
            .iter()
            .position(|(id, owner)| *id == call.id && *owner == call.owner)
        {
            self.unborn.remove(i);
            // The entry is equal to the call's id and owner.
            self.unborn_bytes -= call.id.as_str().len() + call.owner.0.len();
            inv.mark_incomplete();
        }
        self.by_seen.insert((inv.indexed, key.clone()));
        if inv.minted {
            self.minted_by_seen.insert((inv.indexed, key.clone()));
        }
        self.invocations.insert(key.clone(), Box::new(inv));
        self.recount(&key);
        Some(key)
    }

    fn call(&mut self, call: &Call) {
        self.now = self.now.max(call.at);
        let Some(key) = self.route(call) else {
            return;
        };
        let grows = self.record(&key).seen_bytes(call);
        if self.make_room(Some(&key), grows) {
            self.record(&key).seen(call);
        } else {
            let inv = self.record(&key);
            inv.mark_incomplete();
            // A reader is authorized on the record's instance names: a call under a
            // name it could not take changes nothing a page shows (its counts,
            // `last_seen`, rows or operations in flight), as a dropped call does not.
            if !inv.instances.contains(&call.instance) {
                return;
            }
            inv.touch(call.at);
        }
        // Indexed under its new `last_seen` before any room is made for its row.
        self.recount(&key);
        let inv = self.record(&key);
        match &call.kind {
            CallKind::Uploaded(n) => inv.counts.bytes_uploaded += n,
            CallKind::Downloaded(n) => inv.counts.bytes_downloaded += n,
            CallKind::FindMissingBlobs { asked, missing } => {
                inv.counts.blobs_asked += asked;
                inv.counts.blobs_missing += missing;
            }
            CallKind::GetActionResult { hit: None, .. } => inv.counts.ac_miss += 1,
            CallKind::GetActionResult {
                action,
                hit: Some(listing),
            } => {
                inv.counts.ac_hit += 1;
                // A hit adds a row only for an action the record has not seen.
                if inv.row_of(action).is_none() {
                    let mut row = ActionRow::new(*action, call, How::CacheHit, None);
                    row.outcome = listing.exit_code.map(|exit_code| Outcome::Ok { exit_code });
                    row.set_listing(listing.clone(), OUTPUTS_PER_ROW);
                    if let Some(i) = self.add_row(&key, row) {
                        self.hit(&key, i, call.at);
                    }
                }
            }
            CallKind::Execute { action, answer } => self.execute(&key, call, *action, answer),
        }
        self.recount(&key);
    }

    /// An Execute call of `key`'s record.
    fn execute(&mut self, key: &RecordKey, call: &Call, action: Digest, answer: &ExecuteAnswer) {
        let inv = self.record(key);
        inv.counts.execute += 1;
        let (how, name, operation) = match answer {
            ExecuteAnswer::Cached { name, result } => {
                inv.counts.cache_hit_execute += 1;
                (How::CacheHitExecute, name, Err(result))
            }
            ExecuteAnswer::Joined { name, operation } => {
                inv.counts.joined += 1;
                (How::Joined, name, Ok(*operation))
            }
            ExecuteAnswer::Queued { name, operation } => {
                inv.counts.ran += 1;
                (How::Ran, name, Ok(*operation))
            }
        };
        // A row shows the last Execute of its action: a later one replaces it.
        let row = ActionRow::new(action, call, how, Some(name.clone()));
        let index = match inv.row_of(&action) {
            Some(i) => self.replace_row(key, i, row),
            None => self.add_row(key, row),
        };
        // A cached answer is the action's end; any other waits on an operation.
        let operation = match operation {
            Ok(operation) => operation,
            Err(result) => {
                let inv = self.record(key);
                inv.counts.failed += u64::from(result.exit_code.is_some_and(|c| c != 0));
                if let Some(i) = index {
                    inv.rows[i].outcome =
                        result.exit_code.map(|exit_code| Outcome::Ok { exit_code });
                    self.fill_listing(key, i, (**result).clone());
                    self.hit(key, i, call.at);
                }
                return;
            }
        };
        let waiter = Waiter {
            key: key.clone(),
            row: index,
            name: name.clone(),
        };
        // Counted as if the operation were new: making room may remove it.
        if !self.make_room(Some(key), OPERATION_BYTES + waiter.bytes()) {
            self.record(key).mark_incomplete();
            return;
        }
        self.record(key).in_flight += 1;
        let op = self
            .operations
            .entry(operation)
            .or_insert_with(|| Operation {
                queued: call.at,
                attempts: Vec::new(),
                waiters: Vec::new(),
                counted: 0,
            });
        op.waiters.reserve_exact(1);
        op.waiters.push(waiter);
        // A joined action starts with its twin's attempts so far.
        let attempts = op.attempts.clone();
        self.recount_operation(operation);
        if let Some(i) = index {
            self.fill_attempts(key, i, &attempts);
        }
    }

    /// Adds `row` to `key`'s record if it takes rows and the row fits within the
    /// bound (making room as the type's docs say); its index, or `None` with the
    /// record marked `rows_capped`.
    fn add_row(&mut self, key: &RecordKey, row: ActionRow) -> Option<usize> {
        let cap = self.row_cap;
        let cost = self.record(key).row_cost(&row);
        if !self.record(key).takes_rows(cap) || !self.make_room(Some(key), cost) {
            self.record(key).rows_capped = true;
            return None;
        }
        let i = self.record(key).push_row(row);
        self.recount(key);
        Some(i)
    }

    /// Replaces row `i` of `key`'s record with `row` if it fits; its index, or `None`
    /// with the row left as it was and the record marked incomplete.
    fn replace_row(&mut self, key: &RecordKey, i: usize, row: ActionRow) -> Option<usize> {
        let had = self.record(key).row_bytes[i];
        if !self.make_room(Some(key), row.heap_bytes().saturating_sub(had)) {
            self.record(key).mark_incomplete();
            return None;
        }
        let inv = self.record(key);
        inv.rows[i] = row;
        inv.refresh_row(i);
        self.recount(key);
        Some(i)
    }

    /// Copies `attempts` into row `i` of `key`'s record if they fit; else the row
    /// keeps the attempts it had and the record is marked incomplete.
    fn fill_attempts(&mut self, key: &RecordKey, i: usize, attempts: &[Attempt]) {
        let had = attempts_bytes(&self.record(key).rows[i].attempts);
        if self.make_room(Some(key), attempts_bytes(attempts).saturating_sub(had)) {
            let inv = self.record(key);
            // The operation's attempts carry no history id: each row's are its own, set
            // once (`Records::set_run_id`), and kept across the copies that follow.
            let held = std::mem::take(&mut inv.rows[i].attempts);
            let mut copy = attempts.to_vec();
            for a in &mut copy {
                a.run_id = held
                    .iter()
                    .find(|h| h.lease == a.lease)
                    .and_then(|h| h.run_id);
            }
            inv.rows[i].attempts = copy;
            inv.refresh_row(i);
        } else {
            self.record(key).mark_incomplete();
        }
        self.recount(key);
    }

    /// Sets `outcome` on row `i` of `key`'s record, without its text if that does not
    /// fit (the record then marked incomplete).
    fn fill_outcome(&mut self, key: &RecordKey, i: usize, outcome: &Outcome) {
        let mut outcome = outcome.clone();
        if let Some(text) = outcome.text_mut()
            && !self.make_room(Some(key), text.len())
        {
            // Cleared, a string would keep its allocation.
            *text = String::new();
            self.record(key).mark_incomplete();
        }
        let inv = self.record(key);
        inv.rows[i].outcome = Some(outcome);
        inv.refresh_row(i);
        self.recount(key);
    }

    /// Copies `listing` into row `i` of `key`'s record with as many of its outputs as
    /// fit (past the row's limit, or past the bound, the row says
    /// `outputs_truncated`; past the bound the record is also marked incomplete).
    fn fill_listing(&mut self, key: &RecordKey, i: usize, listing: Listing) {
        let rowed = || listing.outputs.iter().take(OUTPUTS_PER_ROW);
        let mut keep = OUTPUTS_PER_ROW;
        if !self.make_room(Some(key), rowed().map(output_bytes).sum()) {
            let mut room = self.bound - self.bytes;
            keep = rowed()
                .take_while(|o| match room.checked_sub(output_bytes(o)) {
                    Some(left) => {
                        room = left;
                        true
                    }
                    None => false,
                })
                .count();
            self.record(key).mark_incomplete();
        }
        let inv = self.record(key);
        inv.rows[i].set_listing(listing, keep);
        inv.refresh_row(i);
        self.recount(key);
    }

    /// Copies `operation`'s attempts into every row that waits on it, and marks each
    /// waiting record seen at `at`.
    fn spread(&mut self, operation: OperationId, at: WallTime) {
        let op = &self.operations[&operation];
        let (attempts, waiters) = (op.attempts.clone(), op.waiters.clone());
        self.spread_to(&waiters, &attempts, at);
    }

    /// Copies `attempts` into each of `waiters`' rows and marks each one's record
    /// seen at `at`; a waiter whose record making room removed is passed over.
    fn spread_to(&mut self, waiters: &[Waiter], attempts: &[Attempt], at: WallTime) {
        self.now = self.now.max(at);
        for w in waiters {
            let Some(inv) = self.invocations.get_mut(&w.key) else {
                continue;
            };
            inv.touch(at);
            let row = w.row(inv);
            self.recount(&w.key);
            if let Some(i) = row {
                self.fill_attempts(&w.key, i, attempts);
            }
        }
    }

    fn finish(&mut self, f: Finished) {
        let Some(mut op) = self.operations.remove(&f.operation) else {
            return;
        };
        // Its bytes go first: what it held makes room for its rows' fill-in.
        self.bytes -= op.counted;
        if let Some((lease, end)) = &f.lease
            && let Some(a) = lease_of(&mut op.attempts, *lease)
        {
            a.ended = Some(f.at);
            a.end = Some(match end {
                LeaseEnd::Accepted => AttemptEnd::Accepted,
                LeaseEnd::Failed(failure, reason) => {
                    AttemptEnd::Failed(*failure, capped(reason, REASON_CAP))
                }
            });
        }
        self.spread_to(&op.waiters, &op.attempts, f.at);
        let outcome = match f.outcome {
            Outcome::Failed { code, message } => Outcome::Failed {
                code,
                message: capped(&message, REASON_CAP),
            },
            Outcome::Refused(reason) => Outcome::Refused(capped(&reason, REASON_CAP)),
            other => other,
        };
        for w in &op.waiters {
            let Some(inv) = self.invocations.get_mut(&w.key) else {
                continue;
            };
            inv.in_flight -= 1;
            inv.counts.failed += u64::from(outcome.is_failure());
            let row = w.row(inv);
            self.recount(&w.key);
            if let Some(i) = row {
                self.fill_outcome(&w.key, i, &outcome);
                if let Some(listing) = &f.result {
                    self.fill_listing(&w.key, i, listing.clone());
                }
                if let Some((lease, _)) = &f.lease {
                    self.note_ended(&w.key, i, *lease, EndedKind::Finished, f.at);
                }
            }
        }
    }

    /// Counts `key`'s record again toward the bound, and indexes it under its
    /// `last_seen`.
    fn recount(&mut self, key: &RecordKey) {
        let inv = self.invocations.get_mut(key).expect(HELD);
        let bytes = inv.bytes();
        self.bytes = self.bytes - inv.counted + bytes;
        inv.counted = bytes;
        if inv.indexed != inv.last_seen {
            let (old, new) = ((inv.indexed, key.clone()), (inv.last_seen, key.clone()));
            inv.indexed = inv.last_seen;
            if inv.minted {
                self.minted_by_seen.remove(&old);
                self.minted_by_seen.insert(new.clone());
            }
            self.by_seen.remove(&old);
            self.by_seen.insert(new);
        }
    }

    /// Counts `operation`, which this holds, again toward the bound.
    fn recount_operation(&mut self, operation: OperationId) {
        let op = self.operations.get_mut(&operation).expect(HELD);
        let bytes = op.bytes();
        self.bytes = self.bytes - op.counted + bytes;
        op.counted = bytes;
    }

    /// Removes records, in the order of the type's docs, until `cost` more bytes fit
    /// within the bound; never `keep`'s. `false`, with the largest open record closed,
    /// if they do not.
    fn make_room(&mut self, keep: Option<&RecordKey>, cost: usize) -> bool {
        while self.bytes.saturating_add(cost) > self.bound {
            match self.victim(keep) {
                Some(victim) => self.evict(&victim),
                None => {
                    self.close_largest();
                    return false;
                }
            }
        }
        true
    }

    /// The next record to remove for the bound, other than `keep`: the least recently
    /// seen minted record, or else the least recently seen idle one.
    fn victim(&self, keep: Option<&RecordKey>) -> Option<RecordKey> {
        let minted = self.minted_by_seen.iter().map(|(_, k)| k);
        let idle = self
            .by_seen
            .iter()
            .map(|(_, k)| (k, &self.invocations[k]))
            // A record with nothing in flight that is not idle was seen too recently,
            // and so was every record after it.
            .take_while(|(_, inv)| inv.in_flight > 0 || inv.state(self.now) == State::Idle)
            .filter(|(_, inv)| inv.in_flight == 0)
            .map(|(k, _)| k);
        minted.chain(idle).find(|k| Some(*k) != keep).cloned()
    }

    /// Removes `key`'s record, and its calls from the operations they wait on.
    fn evict(&mut self, key: &RecordKey) {
        let inv = self.invocations.remove(key).expect(HELD);
        self.bytes -= inv.counted;
        self.by_seen.remove(&(inv.indexed, key.clone()));
        self.minted_by_seen.remove(&(inv.indexed, key.clone()));
        let family = self.ids.get_mut(key.id()).expect(HELD);
        family.keys.retain(|k| k != key);
        family.keys.shrink_to_fit();
        if family.keys.is_empty() {
            self.ids.remove(key.id());
        }
        let owner = RecordKey::owner(key.id().clone());
        if let Some(inv) = self.invocations.get_mut(&owner) {
            inv.collisions.retain(|k| k != key);
            inv.collisions.shrink_to_fit();
            self.recount(&owner);
        }
        if inv.in_flight > 0 {
            let mut freed = 0;
            self.operations.retain(|_, op| {
                op.waiters.retain(|w| w.key != *key);
                // Give back what the removed calls took.
                op.waiters.shrink_to_fit();
                let left = if op.waiters.is_empty() { 0 } else { op.bytes() };
                freed += op.counted - left;
                op.counted = left;
                left > 0
            });
            self.bytes -= freed;
        }
        if !inv.minted {
            self.evicted_seen = self.evicted_seen.max(Some(inv.last_seen));
        }
        self.evicted += 1;
    }

    /// Stops the largest open record that still takes rows from taking more.
    fn close_largest(&mut self) {
        let largest = self
            .invocations
            .values_mut()
            .filter(|inv| !inv.rows_closed)
            .max_by_key(|inv| inv.counted);
        if let Some(inv) = largest {
            inv.rows_closed = true;
            inv.rows_capped = true;
        }
    }
}

/// The attempt under `lease`.
fn lease_of(attempts: &mut [Attempt], lease: LeaseId) -> Option<&mut Attempt> {
    attempts.iter_mut().find(|a| a.lease == lease)
}
