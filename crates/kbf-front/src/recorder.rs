//! The invocation recorder (`docs/design/invocations.md`, sections 3.4 and 3.5, stage
//! 1): the seams of the front and the farm send it [`Event`]s, and its task builds the
//! records from them, in memory, within `--invocation-memory`.
//!
//! - **It never slows a call.** [`Recorder::emit`] puts an event on a bounded channel
//!   and returns. The channel holds at most [`RECORDER_CHANNEL`] events and, together,
//!   as many bytes ([`Event::bytes`]) as the records' bound; a full channel drops the
//!   event (counted:
//!   `kbf_invocation_events_dropped`, [`Recorder::dropped`]) and leaves a mark naming
//!   the record it was for, which the task marks incomplete once it has applied every
//!   event sent before the drop (so the record, or the operation, is there to mark).
//!   The mark is taken only if its lock is free at once; otherwise every record is
//!   marked.
//! - **The records are `kbf_invocation::Records`**, bounded as that type says: minted,
//!   then idle records are removed at the bound, then the largest open one stops
//!   taking rows. Records are lost on restart.
//! - **Calls without an invocation id are not recorded** (the id-less fallback that
//!   mints one, section 2.3, comes with the link it is for): such a call costs a
//!   branch, and nothing of it is built.
//!
//! The task is a future, [`Recording::run`], that the server spawns as a task of its
//! own. A [`Recorder::off`] records nothing and costs a branch per call.

use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_auth::AuthenticationMetadata;
use kbf_invocation::{
    ActionRow, Call, CallKind, Ended, Event, ExecuteAnswer, Invocation, InvocationId, Listing,
    Metadata, OUTPUTS_PER_ROW, Output, Owner, ROW_CAP, RecordKey, Records, Usage, WallTime,
};
use kbf_proto::reapi;
use kbf_proto::worker::ResourceUsage;
use kbf_types::{Digest, OperationId};
use prost::Message;
use tokio::sync::{Notify, mpsc};

use crate::request_metadata::{CallMetadata, Text};
use crate::wire;

/// The default memory bound of the records, `--invocation-memory` (section 3.5).
pub const DEFAULT_INVOCATION_MEMORY: usize = 256 << 20;

/// Events the channel holds before it drops one.
pub const RECORDER_CHANNEL: usize = 65_536;

/// The type URL of a `kbf.worker.v1.ResourceUsage` in a result's
/// `execution_metadata.auxiliary_metadata`.
const USAGE_TYPE_URL: &str = "type.googleapis.com/kbf.worker.v1.ResourceUsage";

/// Drop marks held before the task takes them; past it every record is marked.
const MARKS_CAP: usize = 1024;

/// How often the task asks its records which have gone idle, when it has a [`Tap`].
const IDLE_SWEEP: Duration = Duration::from_secs(10);

/// Where events go. Cheap to clone; every clone sends to the same task.
#[derive(Clone, Debug, Default)]
pub struct Recorder(Option<Arc<Shared>>);

/// The task that applies events, made with its [`Recorder`].
#[derive(Debug)]
pub struct Recording {
    events: mpsc::Receiver<(usize, Event)>,
    shared: Arc<Shared>,
    tap: Option<Arc<dyn Tap>>,
}

/// What reads the records as they settle, for history that outlives them: the
/// [`Recording`] task calls it after it applies each event and, every ten seconds,
/// for the records that went idle. It is installed with [`Recording::with_tap`].
///
/// **It never blocks.** Both methods run on the task, under the lock of the records
/// the build pages read, so a slow tap delays the page reads and every later event,
/// and (once the channel is full) drops them. Do no I/O, take no lock a request
/// path takes, wait for nobody, and do not call back into the [`Recorder`] (its
/// reads take the lock the call already holds). Hand the work to another task
/// through a bounded queue that drops and counts when it is full. A panic ends the
/// recording task.
pub trait Tap: Send + Sync + Debug {
    /// Something settled (see [`Ended`]): the attempt at `ended.attempt` of `row`, or
    /// the cache hit that gave it. Returns the id to keep on the attempt (opaque bytes
    /// the tap minted for it), if it minted one; the task writes it into the record, so
    /// that the page of the row can name the run.
    fn ended(&self, ended: &Ended, row: &ActionRow) -> Option<[u8; 12]>;

    /// The record under `key` has been quiet for `IDLE_AFTER` with no operation in
    /// flight; reported once per quiet spell.
    fn idle(&self, key: &RecordKey);
}

#[derive(Debug)]
struct Shared {
    /// Each event with its [`Event::bytes`].
    events: mpsc::Sender<(usize, Event)>,
    /// The bytes the channel may hold.
    channel_bytes: usize,
    /// The bytes of the events on the channel.
    queued_bytes: AtomicUsize,
    records: Mutex<Records>,
    /// Events put on the channel.
    sent: AtomicU64,
    /// Events the task has applied.
    applied: AtomicU64,
    /// Events dropped on a full channel.
    dropped: AtomicU64,
    /// What the dropped events were for, for the task to mark.
    marks: Mutex<Marks>,
    /// A drop could not leave its mark: the task marks every record once it has
    /// applied this many events, plus one (0: no such drop).
    mark_all: AtomicU64,
    /// Wakes the task to take new marks.
    marked: Notify,
    /// Wakes [`Recorder::caught_up`] after each event.
    progress: Notify,
}

/// The records dropped events were for, each with the number of events sent before
/// it was dropped.
#[derive(Debug, Default)]
struct Marks {
    calls: Vec<(u64, InvocationId, Owner)>,
    operations: Vec<(u64, OperationId)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing under these locks panics midway through a change.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Recorder {
    /// A recorder whose records stay within `memory` bytes, on a channel of
    /// `channel` events and `memory` bytes, and the task that applies them.
    #[must_use]
    pub fn new(memory: usize, channel: usize) -> (Self, Recording) {
        let (sender, events) = mpsc::channel(channel);
        let shared = Arc::new(Shared {
            events: sender,
            channel_bytes: memory,
            queued_bytes: AtomicUsize::new(0),
            records: Mutex::new(Records::with_bound(ROW_CAP, memory)),
            sent: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            marks: Mutex::default(),
            mark_all: AtomicU64::new(0),
            marked: Notify::new(),
            progress: Notify::new(),
        });
        let recording = Recording {
            events,
            shared: Arc::clone(&shared),
            tap: None,
        };
        (Self(Some(shared)), recording)
    }

    /// A recorder that records nothing.
    #[must_use]
    pub const fn off() -> Self {
        Self(None)
    }

    /// Whether events are recorded.
    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// Sends `event` to the task without waiting: on a full channel (of events, or
    /// of bytes) it is dropped and its record marked incomplete.
    pub fn emit(&self, event: Event) {
        let Some(shared) = &self.0 else {
            return;
        };
        let bytes = event.bytes();
        let queued = shared.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
        if queued + bytes > shared.channel_bytes {
            shared.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
            shared.drop_event(&event);
            return;
        }
        match shared.events.try_send((bytes, event)) {
            Ok(()) => {
                shared.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full((_, event))) => {
                shared.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                shared.drop_event(&event);
            }
            // The task is gone: the server is stopping, and nothing reads the count.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Sends a call of the invocation `metadata` names, made by `caller` on
    /// `instance`, answered as `kind`; a call without an invocation id is not
    /// recorded. `kind` is built only for a call that is: with the recorder on and
    /// an id. (A `dyn` closure, so that every call site runs one copy of this
    /// function.)
    pub fn call(
        &self,
        metadata: &CallMetadata,
        caller: &AuthenticationMetadata,
        instance: &str,
        kind: &dyn Fn() -> CallKind,
    ) {
        let recorded = self.is_on() && metadata.invocation_id.is_some();
        if let Some(call) = recorded
            .then(kind)
            .and_then(|kind| call(metadata, caller, instance, kind))
        {
            self.emit(Event::Call(Box::new(call)));
        }
    }

    /// Events dropped on a full channel: `kbf_invocation_events_dropped`.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |s| s.dropped.load(Ordering::Relaxed))
    }

    /// Events put on the channel.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |s| s.sent.load(Ordering::Relaxed))
    }

    /// A copy of the record under `key`, as the task has built it so far.
    #[must_use]
    pub fn get(&self, key: &RecordKey) -> Option<Invocation> {
        lock(&self.0.as_ref()?.records).get(key).cloned()
    }

    /// The latest `last_seen` of a client's record removed for the memory bound
    /// ([`Records::evicted_last_seen`]); `None` when none was, or the recorder is off.
    #[must_use]
    pub fn evicted_last_seen(&self) -> Option<WallTime> {
        self.0
            .as_ref()
            .and_then(|s| lock(&s.records).evicted_last_seen())
    }

    /// Runs `read` on the record under `key`, as the task has built it so far, and
    /// returns `true`; `false`, without running it, when there is no such record (or
    /// the recorder is off). It runs under the records' lock, which the task waits
    /// for: keep it short.
    pub fn read(&self, key: &RecordKey, read: &mut dyn FnMut(&Invocation)) -> bool {
        self.0.as_ref().is_some_and(|shared| {
            let records = lock(&shared.records);
            records.get(key).map(read).is_some()
        })
    }

    /// The records' bytes together, and the bound.
    #[must_use]
    pub fn bytes(&self) -> (usize, usize) {
        self.0.as_ref().map_or((0, 0), |s| {
            let records = lock(&s.records);
            (records.bytes(), records.bound())
        })
    }

    /// Completes once the task has applied every event sent so far, and the marks of
    /// the events dropped before the last of them.
    pub async fn caught_up(&self) {
        let Some(shared) = &self.0 else {
            return;
        };
        loop {
            let progress = shared.progress.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            if shared.applied.load(Ordering::Acquire) >= shared.sent.load(Ordering::Acquire) {
                return;
            }
            progress.await;
        }
    }
}

impl Marks {
    fn len(&self) -> usize {
        self.calls.len() + self.operations.len()
    }
}

impl Shared {
    /// Counts a dropped event and leaves its mark, never waiting for the lock.
    fn drop_event(&self, event: &Event) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        tracing::debug!("kbf_invocation_events_dropped: the recorder's channel is full");
        let after = self.sent.load(Ordering::Acquire);
        let marked = self.marks.try_lock().ok().filter(|m| m.len() < MARKS_CAP);
        match marked {
            Some(mut marks) => match event {
                Event::Call(call) => {
                    marks
                        .calls
                        .push((after, call.id.clone(), call.owner.clone()));
                }
                Event::Command { id, owner, .. } => {
                    marks.calls.push((after, id.clone(), owner.clone()));
                }
                Event::Granted { operation, .. }
                | Event::Started { operation, .. }
                | Event::Requeued { operation, .. }
                | Event::Ran { operation, .. } => marks.operations.push((after, *operation)),
                Event::Finished(f) => marks.operations.push((after, f.operation)),
            },
            None => {
                self.mark_all.fetch_max(after + 1, Ordering::AcqRel);
            }
        }
        self.marked.notify_one();
    }

    /// Marks the records the dropped events were for, of each drop whose earlier
    /// events have all been applied (`applied` of them).
    fn take_marks(&self, records: &mut Records, applied: u64) {
        let mut marks = lock(&self.marks);
        marks.calls.retain(|(after, id, owner)| {
            let ready = *after <= applied;
            if ready {
                records.mark_incomplete_call(id, owner);
            }
            !ready
        });
        marks.operations.retain(|(after, operation)| {
            let ready = *after <= applied;
            if ready {
                records.mark_incomplete_operation(*operation);
            }
            !ready
        });
        let all = self.mark_all.load(Ordering::Acquire);
        let ready = all.checked_sub(1).is_some_and(|after| after <= applied);
        if ready {
            // A drop since the load raised it: it stays, for its own turn.
            let _ = self
                .mark_all
                .compare_exchange(all, 0, Ordering::AcqRel, Ordering::Acquire);
            records.mark_all_incomplete();
        }
    }
}

impl Recording {
    /// This task with `tap` reading the records as they settle ([`Tap`]). Without one
    /// the task does nothing beyond applying events: no sweep, no extra wake-up.
    #[must_use]
    pub fn with_tap(self, tap: Arc<dyn Tap>) -> Self {
        Self {
            tap: Some(tap),
            ..self
        }
    }

    /// Applies events as they come, and marks the records of dropped ones. It runs
    /// as long as the server: the channel never closes, since this task holds a
    /// sender of its own.
    pub async fn run(mut self) {
        let mut sweep = self.tap.as_ref().map(|_| {
            let start = tokio::time::Instant::now() + IDLE_SWEEP;
            let mut sweep = tokio::time::interval_at(start, IDLE_SWEEP);
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            sweep
        });
        loop {
            // Queued events first: a drop's mark waits for the events sent before it. Without
            // a tap the sweep's future never completes (select! builds a disabled branch's
            // future too, so it cannot be an `expect`).
            let (event, swept) = tokio::select! {
                biased;
                event = self.events.recv() => (event, false),
                () = self.shared.marked.notified() => (None, false),
                () = async {
                    match sweep.as_mut() {
                        Some(sweep) => {
                            sweep.tick().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => (None, true),
            };
            {
                let mut records = lock(&self.shared.records);
                let mut applied = self.shared.applied.load(Ordering::Acquire);
                if let Some((bytes, event)) = event {
                    let ended = records.apply(event);
                    self.shared.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                    applied += 1;
                    if let Some(tap) = &self.tap {
                        tell(tap.as_ref(), &mut records, &ended);
                    }
                }
                self.shared.take_marks(&mut records, applied);
                if swept {
                    let idle = records.idle_since(wall_now());
                    let report = |tap: &Arc<dyn Tap>| idle.iter().for_each(|key| tap.idle(key));
                    self.tap.iter().for_each(report);
                }
                self.shared.applied.store(applied, Ordering::Release);
            }
            self.shared.progress.notify_waiters();
        }
    }
}

/// Tells `tap` what `ended` (the end of one applied event), and writes into each
/// attempt the id the tap kept for it. A record the event's own admission removed
/// from the bound is passed over.
fn tell(tap: &dyn Tap, records: &mut Records, ended: &[Ended]) {
    for e in ended {
        let told = records
            .get(&e.key)
            .and_then(|inv| inv.rows().get(e.row))
            .and_then(|row| {
                let lease = e.attempt.and_then(|a| row.attempts.get(a)).map(|a| a.lease);
                tap.ended(e, row).zip(lease)
            });
        if let Some((id, lease)) = told {
            records.set_run_id(&e.key, e.row, lease, id);
        }
    }
}

/// An Execute call to record once the dispatcher has queued it or joined it to a
/// twin, which only the dispatcher knows. It rides in the [`crate::Submission`], and
/// the dispatcher sends [`ExecuteCall::answered`] before anything of the operation can
/// happen (a lease grant), so the recorder sees the call before the operation's
/// events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecuteCall {
    /// The call, its `kind` the queued answer under a name and operation not yet
    /// known.
    call: Call,
    action: Digest,
    command: Digest,
    /// The shape the dispatcher read from the Command, if it read one.
    shape: Option<Box<kbf_invocation::ActionShape>>,
}

/// What an Execute call's `RequestMetadata` says the action is: its kind (`CppCompile`)
/// and the target it belongs to, as the client sent them (capped, see `Text`). The
/// server's `action usage:` line names them ([`ActionLabel::sent`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionLabel {
    /// `action_mnemonic`, when sent.
    pub mnemonic: Option<String>,
    /// `target_id`, when sent.
    pub target: Option<String>,
}

impl ExecuteCall {
    /// The call of `action` (whose Action names `command`) that `metadata` names, by
    /// `caller` on `instance`; `None` without an invocation id or with `recorder` off.
    #[must_use]
    pub fn new(
        recorder: &Recorder,
        metadata: &CallMetadata,
        caller: &AuthenticationMetadata,
        instance: &str,
        action: Digest,
        command: Digest,
    ) -> Option<Self> {
        if !recorder.is_on() {
            return None;
        }
        let kind = CallKind::Execute {
            action,
            answer: ExecuteAnswer::Queued {
                name: String::new(),
                operation: OperationId(0),
            },
        };
        let call = call(metadata, caller, instance, kind)?;
        Some(Self {
            call,
            action,
            command,
            shape: None,
        })
    }

    /// The call, with the `shape` the dispatcher read from its Command, which its row
    /// takes with the Command digest.
    #[must_use]
    pub fn with_shape(self, shape: kbf_invocation::ActionShape) -> Self {
        Self {
            shape: Some(Box::new(shape)),
            ..self
        }
    }

    /// The events of the call, answered with the operation name `name` on the
    /// scheduler's `operation`, which it `joined` or queued: the call, then the
    /// Command digest (and shape) its row takes.
    #[must_use]
    pub fn answered(self, name: String, operation: OperationId, joined: bool) -> [Event; 2] {
        let Self {
            mut call,
            action,
            command,
            shape,
        } = self;
        let answer = if joined {
            ExecuteAnswer::Joined { name, operation }
        } else {
            ExecuteAnswer::Queued { name, operation }
        };
        call.kind = CallKind::Execute { action, answer };
        let command = Event::Command {
            id: call.id.clone(),
            owner: call.owner.clone(),
            action,
            command,
            shape,
        };
        [Event::Call(Box::new(call)), command]
    }
}

/// The record's [`Call`] for a call `metadata` names: `None` without an invocation id.
fn call(
    metadata: &CallMetadata,
    caller: &AuthenticationMetadata,
    instance: &str,
    kind: CallKind,
) -> Option<Call> {
    let id = metadata.invocation_id.as_ref()?;
    let string = |t: &Text| (!t.value.is_empty()).then(|| t.value.clone());
    let text = |t: &Text| {
        (!t.value.is_empty()).then(|| kbf_invocation::Text {
            value: t.value.clone(),
            truncated: t.truncated,
        })
    };
    Some(Call {
        id: InvocationId::new(id.as_str()),
        minted: false,
        owner: owner(caller),
        instance: instance.to_owned(),
        at: wall_now(),
        metadata: Metadata {
            tool: string(&metadata.tool_name),
            tool_version: string(&metadata.tool_version),
            correlated_invocations_id: string(&metadata.correlated_invocations_id),
            target: text(&metadata.target_id),
            mnemonic: text(&metadata.action_mnemonic),
            configuration: text(&metadata.configuration_id),
            action_id: text(&metadata.action_id),
        },
        kind,
    })
}

/// A caller's `public` authentication metadata as one line of JSON, empty without
/// it: the record's owner (section 2.4).
#[must_use]
pub fn owner(caller: &AuthenticationMetadata) -> Owner {
    Owner(
        caller
            .public()
            .map_or_else(String::new, ToString::to_string),
    )
}

/// Now on the wall clock, as events carry it.
#[must_use]
pub fn wall_now() -> WallTime {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    WallTime::from_unix_micros(u64::try_from(since.as_micros()).unwrap_or(u64::MAX))
}

/// What a row copies from the ActionResult stored as `result` (`None` for a
/// timed-out run's partial result, which is never stored) (section 3.2): its
/// output files and directories (files first, each in the result's order; one whose
/// digest does not parse is left out), its stdout and stderr digests and the
/// `ResourceUsage` it carries. It lists at most one output past the
/// [`OUTPUTS_PER_ROW`] a row keeps, so that the row knows it was cut and an event
/// holds no more.
#[must_use]
pub fn listing(result: Option<Digest>, r: &reapi::ActionResult) -> Listing {
    let files = r.output_files.iter().filter_map(|f| {
        let digest = wire::digest(f.digest.as_ref()).ok()?;
        Some(Output {
            path: f.path.clone(),
            digest,
            executable: f.is_executable,
            directory: false,
        })
    });
    let directories = r.output_directories.iter().filter_map(|d| {
        let digest = wire::digest(d.tree_digest.as_ref()).ok()?;
        Some(Output {
            path: d.path.clone(),
            digest,
            executable: false,
            directory: true,
        })
    });
    let log = |d: Option<&reapi::Digest>| d.and_then(|d| wire::digest(Some(d)).ok());
    Listing {
        result,
        exit_code: result.map(|_| r.exit_code),
        outputs: files.chain(directories).take(OUTPUTS_PER_ROW + 1).collect(),
        stdout: log(r.stdout_digest.as_ref()),
        stderr: log(r.stderr_digest.as_ref()),
        usage: usage(r),
    }
}

/// The `ResourceUsage` a result carries, if one decodes.
fn usage(r: &reapi::ActionResult) -> Option<Usage> {
    let any = r
        .execution_metadata
        .as_ref()?
        .auxiliary_metadata
        .iter()
        .find(|a| a.type_url == USAGE_TYPE_URL)?;
    let u = ResourceUsage::decode(any.value.as_slice()).ok()?;
    Some(Usage {
        cpu_user_micros: u.cpu_user_micros,
        cpu_system_micros: u.cpu_system_micros,
        peak_memory_bytes: u.peak_memory_bytes,
        wall_micros: u.wall_micros,
    })
}
