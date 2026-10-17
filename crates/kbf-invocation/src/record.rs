//! One invocation's record (sections 3.1 and 3.2): its header fields, its counters and
//! its action rows. [`crate::Records`] builds it from events; this module holds the
//! values and the rules that touch one record only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, LeaseId, WorkerId};

use crate::event::{Call, InvocationId, Listing, Metadata, Outcome, Output, Owner, WallTime};
use crate::run::{ActionShape, Run};

/// Rows one record keeps; past it, actions are counted but not rowed (section 3.3).
pub const ROW_CAP: usize = 1_000_000;
/// Outputs one row keeps; past it, `outputs_truncated` (section 3.2).
pub const OUTPUTS_PER_ROW: usize = 1_000;
/// Bytes kept of each `RequestMetadata` text field (section 2.2).
pub const TEXT_CAP: usize = 512;
/// Bytes kept of a reason or status message (section 3.2).
pub const REASON_CAP: usize = 1024;
/// How long an invocation with no operation in flight goes without an event before it
/// is idle (section 3.1).
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// A record's key: the invocation id, or `<id>~<n>` (n from 2) for the n-th caller
/// whose authentication metadata differs from the owner's (section 2.4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    id: InvocationId,
    collision: Option<usize>,
}

impl RecordKey {
    /// The owner's record of `id`.
    #[must_use]
    pub const fn owner(id: InvocationId) -> Self {
        Self {
            id,
            collision: None,
        }
    }

    /// The record of the `n`-th caller of `id` (n from 2).
    #[must_use]
    pub const fn collision(id: InvocationId, n: usize) -> Self {
        Self {
            id,
            collision: Some(n),
        }
    }

    /// The invocation id.
    #[must_use]
    pub const fn id(&self) -> &InvocationId {
        &self.id
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id.as_str())?;
        match self.collision {
            Some(n) => write!(f, "~{n}"),
            None => Ok(()),
        }
    }
}

/// A `RequestMetadata` text field, kept up to [`TEXT_CAP`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    /// The text, or its first [`TEXT_CAP`] bytes cut at a character boundary.
    pub value: String,
    /// Whether the client sent more.
    pub truncated: bool,
}

impl Text {
    /// `value` as the client sent it.
    #[must_use]
    pub fn sent(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            truncated: false,
        }
    }

    /// The field as a row keeps it: `None` when empty, else its first [`TEXT_CAP`]
    /// bytes, truncated if the client sent more (here or before it came in).
    fn kept(value: Option<&Self>) -> Option<Self> {
        let sent = value.filter(|v| !v.value.is_empty())?;
        let value = capped(&sent.value, TEXT_CAP);
        Some(Self {
            truncated: sent.truncated || value.len() < sent.value.len(),
            value,
        })
    }
}

/// The first `cap` bytes of `text`, cut back to a character boundary.
pub(crate) fn capped(text: &str, cap: usize) -> String {
    text[..text.floor_char_boundary(cap)].to_owned()
}

/// How the farm answered an action (section 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// GetActionResult found it in the action cache.
    CacheHit,
    /// Execute was answered from the action cache.
    CacheHitExecute,
    /// Execute attached to a twin's operation already in flight.
    Joined,
    /// Execute queued a new operation.
    Ran,
}

/// One lease of an action's operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The lease.
    pub lease: LeaseId,
    /// The worker it was granted to.
    pub worker: WorkerId,
    /// When the operation was queued for it: submitted, or requeued after the lease
    /// before it.
    pub queued: WallTime,
    /// When it was granted.
    pub granted: WallTime,
    /// When its worker was sent the Start.
    pub started: Option<WallTime>,
    /// When it ended.
    pub ended: Option<WallTime>,
    /// How it ended; `None` while it is held.
    pub end: Option<AttemptEnd>,
    /// What its lease's report measured; `None` without a report the scheduler took.
    pub run: Option<Box<Run>>,
    /// The id the history writer minted for this attempt in this record, as opaque bytes
    /// (`kbf_history::RunId::to_bytes`); `None` until the recorder's tap sets one
    /// ([`crate::Records::set_run_id`]).
    pub run_id: Option<[u8; 12]>,
}

/// How a lease ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptEnd {
    /// Its result was accepted.
    Accepted,
    /// It was given up and the operation queued again.
    Requeued(RequeueReason),
    /// Its worker reported a failure that ended the operation, with the daemon's
    /// reason text (up to [`REASON_CAP`] bytes).
    Failed(Failure, String),
}

/// One action of an invocation (section 3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRow {
    /// The Action digest.
    pub action: Digest,
    /// The call's instance name.
    pub instance: String,
    /// `target_id`, when sent.
    pub target: Option<Text>,
    /// `action_mnemonic`, when sent.
    pub mnemonic: Option<Text>,
    /// `configuration_id`, when sent.
    pub configuration: Option<Text>,
    /// `action_id`, when sent.
    pub action_id: Option<Text>,
    /// How the farm answered it.
    pub how: How,
    /// The REAPI operation; `None` for a GetActionResult hit.
    pub operation: Option<String>,
    /// Every lease of its operation, in grant order; a joined action's are its twin's.
    pub attempts: Vec<Attempt>,
    /// What the caller was answered; `None` while the operation is in flight.
    pub outcome: Option<Outcome>,
    /// The result and its listing, its outputs cut to [`OUTPUTS_PER_ROW`].
    pub listing: Option<Listing>,
    /// Whether the result listed more outputs than the row keeps.
    pub outputs_truncated: bool,
    /// The Command digest, once the recorder has read it from the Action blob.
    pub command: Option<Digest>,
    /// The shape the server read from the Command; `None` until then, and for an
    /// action that was not queued (a cache hit).
    pub shape: Option<Box<ActionShape>>,
}

impl ActionRow {
    pub(crate) fn new(action: Digest, call: &Call, how: How, operation: Option<String>) -> Self {
        let m: &Metadata = &call.metadata;
        Self {
            action,
            instance: call.instance.clone(),
            target: Text::kept(m.target.as_ref()),
            mnemonic: Text::kept(m.mnemonic.as_ref()),
            configuration: Text::kept(m.configuration.as_ref()),
            action_id: Text::kept(m.action_id.as_ref()),
            how,
            operation,
            attempts: Vec::new(),
            outcome: None,
            listing: None,
            outputs_truncated: false,
            command: None,
            shape: None,
        }
    }

    /// Copies the result's listing into the row, keeping the first `keep` of its
    /// outputs, and no more than [`OUTPUTS_PER_ROW`].
    pub(crate) fn set_listing(&mut self, mut listing: Listing, keep: usize) {
        let keep = keep.min(OUTPUTS_PER_ROW);
        self.outputs_truncated = listing.outputs.len() > keep;
        if self.outputs_truncated {
            // Truncating keeps the capacity: give the rest back.
            listing.outputs.truncate(keep);
            listing.outputs.shrink_to_fit();
        }
        self.listing = Some(listing);
    }

    /// The memory the row holds beyond its own slot in the record's row list, as the
    /// bound of section 3.5 counts it: the bytes of its texts, its shape, its attempts
    /// and its outputs. Every list a row holds is allocated to its length (a copy, or cut
    /// back), so lengths are its capacities.
    pub(crate) fn heap_bytes(&self) -> usize {
        let text = |t: &Option<Text>| t.as_ref().map_or(0, |t| t.value.len());
        let outputs: usize = self
            .listing
            .as_ref()
            .map_or(0, |l| l.outputs.iter().map(output_bytes).sum());
        self.instance.len()
            + text(&self.target)
            + text(&self.mnemonic)
            + text(&self.configuration)
            + text(&self.action_id)
            + self.operation.as_ref().map_or(0, String::len)
            + attempts_bytes(&self.attempts)
            + outcome_bytes(self.outcome.as_ref())
            + outputs
            + self.shape.as_deref().map_or(0, shape_bytes)
    }
}

/// What a row's shape takes: its box and its texts.
pub(crate) fn shape_bytes(shape: &ActionShape) -> usize {
    size_of::<ActionShape>() + shape.heap_bytes()
}

/// What an attempt's run takes: its box and its NUMA list.
pub(crate) fn run_bytes(run: &Run) -> usize {
    size_of::<Run>() + run.heap_bytes()
}

/// What a row's `attempts` take beyond the row, as [`ActionRow::heap_bytes`] counts
/// them; an operation's attempts take as much.
pub(crate) fn attempts_bytes(attempts: &[Attempt]) -> usize {
    attempts.iter().map(attempt_bytes).sum()
}

/// What one attempt takes: itself, its worker's name, its failure reason and its run.
pub(crate) fn attempt_bytes(a: &Attempt) -> usize {
    let reason = match &a.end {
        Some(AttemptEnd::Failed(_, why)) => why.len(),
        _ => 0,
    };
    size_of::<Attempt>() + a.worker.as_str().len() + reason + a.run.as_deref().map_or(0, run_bytes)
}

/// What an outcome's text takes.
pub(crate) fn outcome_bytes(outcome: Option<&Outcome>) -> usize {
    match outcome {
        Some(Outcome::Failed { message, .. }) => message.len(),
        Some(Outcome::Refused(why)) => why.len(),
        _ => 0,
    }
}

/// What one output of a row's listing takes.
pub(crate) fn output_bytes(o: &Output) -> usize {
    size_of::<Output>() + o.path.len()
}

/// The copies of a record's id [`crate::Records`] keeps beside the record's own key:
/// its key in the record map, in the last-seen index and in its id's list of keys,
/// and its id as the id map's key. A minted record's key is in one more index.
const RECORD_INDEX_ENTRIES: usize = 4;

/// What a record's entries in the indexes of [`crate::Records`] take beside its id's
/// bytes: its slot in the record map (key and box), in the last-seen index (time and
/// key), in the id map (id and its list) and in its id's list of keys.
const RECORD_ENTRY_BYTES: usize = btree_entry_bytes(size_of::<RecordKey>() + size_of::<usize>())
    + btree_entry_bytes(SEEN_ENTRY)
    + btree_entry_bytes(
        size_of::<InvocationId>() + size_of::<Vec<RecordKey>>() + size_of::<usize>(),
    )
    + size_of::<RecordKey>();

/// A record's slot in a last-seen index: its time and key. A minted record has one in
/// the minted records' index too.
const SEEN_ENTRY: usize = size_of::<WallTime>() + size_of::<RecordKey>();

/// One entry of a record's action index: the digest and the row's index.
const ACTION_ENTRY: usize = size_of::<Digest>() + size_of::<usize>();

/// One slot of a record's row list and of its list of row costs.
const ROW_SLOT: usize = size_of::<ActionRow>() + size_of::<usize>();

/// Entries a standard library B-tree node has room for.
const NODE_SLOTS: usize = 11;

/// Entries a B-tree leaf accounts for, as estimated: a full node splits into one of
/// 6 and one of 5 and moves one entry up, so keys added in order (times, operation
/// ids) leave 6 in each leaf and one above it for each. Keys added out of order fill
/// nodes further, so this counts them high.
const NODE_FILL: usize = 7;

/// One B-tree leaf of `entry`-byte entries: a parent link, two counts, and the slots.
const fn leaf_bytes(entry: usize) -> usize {
    16 + NODE_SLOTS * entry
}

/// One internal B-tree node: a leaf and its 12 edges.
const fn internal_bytes(entry: usize) -> usize {
    leaf_bytes(entry) + (NODE_SLOTS + 1) * size_of::<usize>()
}

/// What a B-tree map or set of `n` entries of `entry` bytes takes in nodes,
/// estimated: one leaf up to 11 entries; past that a leaf per [`NODE_FILL`] entries,
/// and an internal node per [`NODE_FILL`] nodes below it.
pub(crate) const fn btree_bytes(n: usize, entry: usize) -> usize {
    if n == 0 {
        0
    } else if n <= NODE_SLOTS {
        leaf_bytes(entry)
    } else {
        let leaves = n.div_ceil(NODE_FILL);
        leaves * leaf_bytes(entry) + leaves.div_ceil(NODE_FILL) * internal_bytes(entry)
    }
}

/// One entry's share of a large B-tree's nodes, as [`btree_bytes`] estimates them.
pub(crate) const fn btree_entry_bytes(entry: usize) -> usize {
    (leaf_bytes(entry) + internal_bytes(entry) / NODE_FILL).div_ceil(NODE_FILL)
}

/// The capacity a list of `cap` slots, all used, grows to: as the standard library's
/// lists grow, so that a list of rows costs no more copying.
pub(crate) const fn grown(cap: usize) -> usize {
    if cap == 0 { 4 } else { 2 * cap }
}

/// The counters of section 3.1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Execute calls.
    pub execute: u64,
    /// Of those, answered from the action cache.
    pub cache_hit_execute: u64,
    /// Of those, joined to a running twin.
    pub joined: u64,
    /// Of those, queued to run.
    pub ran: u64,
    /// Execute answers that were not OK with exit code 0.
    pub failed: u64,
    /// GetActionResult calls answered with a result.
    pub ac_hit: u64,
    /// GetActionResult calls answered without one.
    pub ac_miss: u64,
    /// FindMissingBlobs digests asked about.
    pub blobs_asked: u64,
    /// FindMissingBlobs digests answered missing.
    pub blobs_missing: u64,
    /// CAS and ByteStream bytes the client sent.
    pub bytes_uploaded: u64,
    /// CAS and ByteStream bytes the client received.
    pub bytes_downloaded: u64,
}

/// Whether an invocation is still being written (section 3.1). There is no
/// "finished": REAPI has no end-of-build call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// An operation is in flight, or the last event is less than [`IDLE_AFTER`] old.
    Open,
    /// No operation in flight and no event for [`IDLE_AFTER`].
    Idle,
}

/// One invocation's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub(crate) key: RecordKey,
    /// The number [`crate::Records`] gave it when it made it: no other record it made has it.
    pub(crate) serial: u64,
    pub(crate) minted: bool,
    pub(crate) owner: Owner,
    pub(crate) tool: Option<String>,
    pub(crate) tool_version: Option<String>,
    pub(crate) correlated_invocations_id: Option<String>,
    pub(crate) instances: BTreeSet<String>,
    pub(crate) first_seen: WallTime,
    pub(crate) last_seen: WallTime,
    pub(crate) complete: bool,
    pub(crate) rows_capped: bool,
    pub(crate) counts: Counts,
    pub(crate) collisions: Vec<RecordKey>,
    pub(crate) rows: Vec<ActionRow>,
    /// Each rowed action's index in `rows`.
    pub(crate) by_action: BTreeMap<Digest, usize>,
    /// Operations this invocation's Execute calls wait on.
    pub(crate) in_flight: usize,
    /// Each row's [`ActionRow::heap_bytes`], as last counted.
    pub(crate) row_bytes: Vec<usize>,
    /// The sum of `row_bytes`.
    pub(crate) rows_bytes: usize,
    /// What [`crate::Records`] counts for this record toward its bound.
    pub(crate) counted: usize,
    /// The `last_seen` [`crate::Records`] indexes this record under.
    pub(crate) indexed: WallTime,
    /// Whether the memory bound stopped this record taking rows (section 3.5, step 3).
    pub(crate) rows_closed: bool,
    /// The `last_seen` at which [`crate::Records::idle_since`] last reported the record.
    pub(crate) idle_reported: Option<WallTime>,
    /// Where a record read back from history ([`Invocation::restored`]) that is not
    /// complete has a hole: the time of the last chunk written. `None` for a live
    /// record, and for a restored one that is complete.
    pub(crate) gap_from: Option<WallTime>,
    /// Whether the record was read back from history, not built from events.
    pub(crate) from_history: bool,
}

/// What a record read back from history says of its invocation as a whole, for
/// [`Invocation::restored`]. History does not keep the first caller's owner, the tool,
/// the correlated id or the call counters that no row shows: a restored record has none
/// of them.
pub struct Restored {
    /// The record's key.
    pub key: RecordKey,
    /// The record's [`Invocation::serial`]: one no record the recorder made has had.
    pub serial: u64,
    /// The instance names its records used.
    pub instances: BTreeSet<String>,
    /// The first record's time.
    pub first_seen: WallTime,
    /// The last record's time.
    pub last_seen: WallTime,
    /// Whether history holds every record: it was closed and dropped none.
    pub complete: bool,
    /// When not complete: the time of the last chunk written, where the hole starts.
    pub gap_from: Option<WallTime>,
    /// Whether more records were held than `rows` has (the reader stopped at its bound).
    pub rows_capped: bool,
}

impl Invocation {
    pub(crate) fn new(key: RecordKey, call: &Call) -> Self {
        Self {
            key,
            serial: 0,
            minted: call.minted,
            owner: call.owner.clone(),
            tool: None,
            tool_version: None,
            correlated_invocations_id: None,
            instances: BTreeSet::new(),
            first_seen: call.at,
            last_seen: call.at,
            complete: true,
            rows_capped: false,
            counts: Counts::default(),
            collisions: Vec::new(),
            rows: Vec::new(),
            by_action: BTreeMap::new(),
            in_flight: 0,
            row_bytes: Vec::new(),
            rows_bytes: 0,
            counted: 0,
            indexed: call.at,
            rows_closed: false,
            idle_reported: None,
            gap_from: None,
            from_history: false,
        }
    }

    /// A record read back from history: `parts` and `rows`, in the order first seen.
    /// It is not minted and holds no operation. Its counters are those the rows show:
    /// `ran`, `cache_hit_execute` and `joined` count the rows of each `how`, `execute`
    /// their sum, `ac_hit` the rows answered by `GetActionResult`, and `failed` the rows
    /// whose outcome is a failure; `ac_miss`, `blobs_*` and `bytes_*` are 0, which history
    /// does not keep. `rows` must name distinct actions.
    #[must_use]
    pub fn restored(parts: Restored, rows: Vec<ActionRow>) -> Self {
        let mut counts = Counts::default();
        for r in &rows {
            match r.how {
                How::CacheHit => counts.ac_hit += 1,
                How::CacheHitExecute => counts.cache_hit_execute += 1,
                How::Joined => counts.joined += 1,
                How::Ran => counts.ran += 1,
            }
            counts.failed += u64::from(r.outcome.as_ref().is_some_and(Outcome::is_failure));
        }
        counts.execute = counts.ran + counts.cache_hit_execute + counts.joined;
        let Restored {
            key,
            serial,
            instances,
            first_seen,
            last_seen,
            complete,
            gap_from,
            rows_capped,
        } = parts;
        let mut inv = Self {
            key,
            serial,
            minted: false,
            owner: Owner::default(),
            tool: None,
            tool_version: None,
            correlated_invocations_id: None,
            instances,
            first_seen,
            last_seen,
            complete,
            rows_capped,
            counts,
            collisions: Vec::new(),
            rows: Vec::with_capacity(rows.len()),
            by_action: BTreeMap::new(),
            in_flight: 0,
            row_bytes: Vec::with_capacity(rows.len()),
            rows_bytes: 0,
            counted: 0,
            indexed: last_seen,
            rows_closed: false,
            idle_reported: None,
            gap_from: gap_from.filter(|_| !complete),
            from_history: true,
        };
        for row in rows {
            inv.push_row(row);
        }
        inv
    }

    /// Takes what any call says about the invocation as a whole.
    pub(crate) fn seen(&mut self, call: &Call) {
        let m = &call.metadata;
        if self.tool.is_none() && m.tool.is_some() {
            self.tool.clone_from(&m.tool);
            self.tool_version.clone_from(&m.tool_version);
        }
        if self.correlated_invocations_id.is_none() {
            self.correlated_invocations_id = m
                .correlated_invocations_id
                .clone()
                .filter(|c| !c.is_empty());
        }
        self.instances.insert(call.instance.clone());
        self.touch(call.at);
    }

    /// What [`Self::seen`] would add to the record's bytes: a new instance name, and
    /// the tool and correlated id the first time they are sent.
    pub(crate) fn seen_bytes(&self, call: &Call) -> usize {
        let m = &call.metadata;
        let len = |o: &Option<String>| o.as_ref().map_or(0, String::len);
        let instance = if self.instances.contains(&call.instance) {
            0
        } else {
            let n = self.instances.len();
            call.instance.len() + btree_bytes(n + 1, size_of::<String>())
                - btree_bytes(n, size_of::<String>())
        };
        let tool = if self.tool.is_none() && m.tool.is_some() {
            len(&m.tool) + len(&m.tool_version)
        } else {
            0
        };
        let correlated = if self.correlated_invocations_id.is_none() {
            len(&m.correlated_invocations_id)
        } else {
            0
        };
        instance + tool + correlated
    }

    pub(crate) fn touch(&mut self, at: WallTime) {
        self.last_seen = self.last_seen.max(at);
    }

    /// The index of `action`'s row, if it has one.
    pub(crate) fn row_of(&self, action: &Digest) -> Option<usize> {
        self.by_action.get(action).copied()
    }

    /// Whether the record takes another row: it holds fewer than `cap` and the memory
    /// bound has not closed it.
    pub(crate) fn takes_rows(&self, cap: usize) -> bool {
        self.rows.len() < cap && !self.rows_closed
    }

    /// What adding `row` costs: the slots its lists grow by when they are full, its
    /// action index entry and its [`ActionRow::heap_bytes`]. Exactly what
    /// [`Self::bytes`] grows by.
    pub(crate) fn row_cost(&self, row: &ActionRow) -> usize {
        let n = self.rows.len();
        let slots = if n == self.rows.capacity() {
            (grown(n) - n) * ROW_SLOT
        } else {
            0
        };
        slots + btree_bytes(n + 1, ACTION_ENTRY) - btree_bytes(n, ACTION_ENTRY) + row.heap_bytes()
    }

    /// Adds `row`, which [`Self::takes_rows`] allowed, and returns its index.
    pub(crate) fn push_row(&mut self, row: ActionRow) -> usize {
        let n = self.rows.len();
        if n == self.rows.capacity() {
            // Grown here, not by `push`, so that the growth [`Self::row_cost`] counted
            // is the growth made.
            self.rows.reserve_exact(grown(n) - n);
            self.row_bytes.reserve_exact(grown(n) - n);
        }
        let bytes = row.heap_bytes();
        self.by_action.insert(row.action, self.rows.len());
        self.rows.push(row);
        self.row_bytes.push(bytes);
        self.rows_bytes += bytes;
        self.rows.len() - 1
    }

    /// Counts row `i` again after it changed.
    pub(crate) fn refresh_row(&mut self, i: usize) {
        let bytes = self.rows[i].heap_bytes();
        self.rows_bytes = self.rows_bytes - self.row_bytes[i] + bytes;
        self.row_bytes[i] = bytes;
    }

    /// The memory the record holds, as the bound of section 3.5 counts it: the record
    /// itself (boxed), its entries in the indexes of [`crate::Records`] and each copy of
    /// its id there, its header's texts and instance set, its row lists' capacity, its
    /// action index and each row's heap bytes (texts, attempts, outputs). An estimate
    /// of what the allocator is asked for: B-tree nodes are estimated (see
    /// `btree_bytes`), and the allocator's own overhead is not counted. A copy's row
    /// lists are allocated to their length, so a copy may count less.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let option = |o: &Option<String>| o.as_ref().map_or(0, String::len);
        let instances: usize = self.instances.iter().map(String::len).sum::<usize>()
            + btree_bytes(self.instances.len(), size_of::<String>());
        let collisions: usize = self
            .collisions
            .iter()
            .map(|k| size_of::<RecordKey>() + k.id().as_str().len())
            .sum();
        let id = self.key.id().as_str().len();
        let minted = if self.minted {
            btree_entry_bytes(SEEN_ENTRY) + id
        } else {
            0
        };
        size_of::<Self>()
            + RECORD_ENTRY_BYTES
            + (1 + RECORD_INDEX_ENTRIES) * id
            + minted
            + self.owner.0.len()
            + option(&self.tool)
            + option(&self.tool_version)
            + option(&self.correlated_invocations_id)
            + instances
            + collisions
            + self.rows.capacity() * size_of::<ActionRow>()
            + self.row_bytes.capacity() * size_of::<usize>()
            + btree_bytes(self.by_action.len(), ACTION_ENTRY)
            + self.rows_bytes
    }

    /// The record's key.
    #[must_use]
    pub const fn key(&self) -> &RecordKey {
        &self.key
    }

    /// A number no other record of the same [`crate::Records`] has had: a record
    /// removed for the bound and made again under the same key gets a new one, so a
    /// reader can tell it from the one it read before.
    #[must_use]
    pub const fn serial(&self) -> u64 {
        self.serial
    }

    /// Whether the server minted the id (section 2.3).
    #[must_use]
    pub const fn minted(&self) -> bool {
        self.minted
    }

    /// The `public` authentication metadata of its first caller.
    #[must_use]
    pub const fn owner(&self) -> &Owner {
        &self.owner
    }

    /// `tool_details` of the first call that sent them: name and version.
    #[must_use]
    pub fn tool(&self) -> (Option<&str>, Option<&str>) {
        (self.tool.as_deref(), self.tool_version.as_deref())
    }

    /// The first non-empty `correlated_invocations_id` seen.
    #[must_use]
    pub fn correlated_invocations_id(&self) -> Option<&str> {
        self.correlated_invocations_id.as_deref()
    }

    /// The instance names its calls used.
    #[must_use]
    pub const fn instances(&self) -> &BTreeSet<String> {
        &self.instances
    }

    /// The first event's time.
    #[must_use]
    pub const fn first_seen(&self) -> WallTime {
        self.first_seen
    }

    /// The last event's time: a call, a grant, a start, a requeue or a finish.
    #[must_use]
    pub const fn last_seen(&self) -> WallTime {
        self.last_seen
    }

    /// Whether the record holds every event of the invocation.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    /// Marks the record as missing events: one was dropped, or a server restarted
    /// while it was open (section 3.4).
    pub const fn mark_incomplete(&mut self) {
        self.complete = false;
    }

    /// Whether the record was read back from history ([`Invocation::restored`]) and not
    /// built from the events of a live invocation.
    #[must_use]
    pub const fn from_history(&self) -> bool {
        self.from_history
    }

    /// For a record read back from history that is not complete: the time of the last
    /// chunk written, where its hole starts. `None` for every other record.
    #[must_use]
    pub const fn gap_from(&self) -> Option<WallTime> {
        self.gap_from
    }

    /// Whether the record stopped taking rows.
    #[must_use]
    pub const fn rows_capped(&self) -> bool {
        self.rows_capped
    }

    /// Its counters.
    #[must_use]
    pub const fn counts(&self) -> &Counts {
        &self.counts
    }

    /// The keys of other callers' records of the same id (the owner's record only).
    #[must_use]
    pub fn collisions(&self) -> &[RecordKey] {
        &self.collisions
    }

    /// Its action rows, in first-seen order.
    #[must_use]
    pub fn rows(&self) -> &[ActionRow] {
        &self.rows
    }

    /// The row of `action`, if it has one.
    #[must_use]
    pub fn row(&self, action: &Digest) -> Option<&ActionRow> {
        self.row_of(action).map(|i| &self.rows[i])
    }

    /// Its state at `now`.
    #[must_use]
    pub fn state(&self, now: WallTime) -> State {
        let quiet = now
            .unix_micros()
            .saturating_sub(self.last_seen.unix_micros());
        if self.in_flight == 0 && u128::from(quiet) >= IDLE_AFTER.as_micros() {
            State::Idle
        } else {
            State::Open
        }
    }
}
