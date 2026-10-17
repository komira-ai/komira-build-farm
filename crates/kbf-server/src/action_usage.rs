//! The server's per-lease `action usage:` log line: what each lease that reported a
//! `ResourceUsage` used, keyed by the action's shape so one target's action can be
//! followed across builds (`docs/design/daemon.md#where-a-leases-time-went`).
//!
//! **Lines.**
//!
//! - `action usage:`, one per accepted report whose result carries a `ResourceUsage`
//!   (an OK run, and a failed one the daemon measured, whatever the outcome: for
//!   example an out-of-memory kill, a timeout, an infrastructure failure, a lost run),
//!   whatever the scheduler does next (a killed
//!   rung runs again higher up the memory ladder; each rung has its line):
//!
//!   ```text
//!   action usage: exit_code=<n|-> mnemonic=<"..."|-> target_hint=<"..."|-> wall_us=<n>
//!     <cgroup fields> key=<16 hex> cfg=<16 hex[-16 hex]|-> target=<"..."|->
//!     target_kind=<target|anon|bxl|-> outcome=<word> lease=<lease> node=<node>
//!   ```
//!
//!   (one line). `exit_code` is the run's, as the daemon reported it for a run it says
//!   ended (an OK status), and `-` for one it reports failed (a killed run has no exit
//!   code). `mnemonic` and `target_hint` are what the client's `RequestMetadata`
//!   said (buck2 says neither); a hint only, never a key. `target` and `target_kind`
//!   are the target the action's own `buck-out` paths name ([`Target`], issue #796),
//!   `-` when they name none. `outcome` is [`outcome`]'s word for the scheduler's
//!   outcome, the server's verdict: an OK run whose outputs the server cannot store
//!   has its exit code and `outcome=infra`.
//! - `action shape:`, the first time this process sees a key: what the key stands for
//!   (`key=… program="…" outputs=<n> patterns="<up to 8>…"`).
//!
//! A cancelled lease's result is refused (its holder is no longer current) and has no
//! line here; the daemon's `lease phases:` line has its usage, and joins on `lease=` to
//! the `lease granted` line, which names the key.
//!
//! **What the farm does** (`crate::farm`), one call at each seam, so a rewrite of the
//! farm carries this module by placing the same calls:
//!
//! 1. `Farm::submit`: [`Shapes::subject`] turns the front's `Described` (the only field
//!    the front adds to its `Submission` for this line) into a [`Subject`] (key, cfg,
//!    label) before the farm's lock is taken, and the subject is kept with the caller
//!    (`Waiter::usage`); the argv and output paths are dropped there;
//! 2. `Farm::report`: [`Measured::of`] reads the report's usage and exit code before
//!    the report is judged (the judging drops a failed run's result);
//! 3. `State::take_report`: once the scheduler took the report, [`log`] writes the
//!    line with the operation's first caller's subject, read before the report is fed
//!    (a report that finishes the operation may drop its callers);
//! 4. `State::offer`: the `lease granted` line names the first caller's key.
//!
//! The key is `shape`'s; nothing else here knows how it is made.

mod shape;

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use kbf_front::{ActionLabel, Described};
use kbf_proto::worker::{self, ResourceUsage};
use kbf_types::{Failure, LeaseId, LostWhy, OperationId, Outcome, WorkerId};
use tonic::Code;

pub use shape::{Shape, ShapeKey, Target, TargetKind};

/// How many keys [`Shapes`] remembers having described before it forgets them all
/// (each is then described once more).
pub const SEEN_KEYS: usize = 65_536;

/// How many output patterns an `action shape:` line shows.
const SHOWN_PATTERNS: usize = 8;

/// What the `action usage:` line says an operation is: its shape's key, the
/// configuration hash and the target beside it, and the label its first caller's
/// `RequestMetadata` named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    /// The shape key.
    pub key: ShapeKey,
    /// The configuration hash an output path named.
    pub cfg: Option<String>,
    /// The target the action's own paths name.
    pub target: Option<Target>,
    /// Whether the action is a test run ([`Shape::test`]).
    pub test: bool,
    /// The mnemonic and target the client sent: hints, never keys.
    pub label: ActionLabel,
}

impl Subject {
    /// The subject of the action `described` names, and its shape; logs nothing (a
    /// farm makes subjects through [`Shapes::subject`]).
    #[must_use]
    pub fn of(described: Described) -> (Self, Shape) {
        let shape = Shape::of(&described.arguments, &described.outputs);
        let subject = Self {
            key: shape.key,
            cfg: shape.cfg.clone(),
            target: shape.target.clone(),
            test: shape.test,
            label: described.label,
        };
        (subject, shape)
    }

    /// The subject as the invocation record keeps it on the action's row: the key,
    /// the configuration hash and the target (the label is on the row already, as the
    /// call's `RequestMetadata`).
    #[must_use]
    pub fn recorded(&self) -> kbf_invocation::ActionShape {
        kbf_invocation::ActionShape {
            key: u64::from_be_bytes(self.key.bytes()),
            cfg: self.cfg.clone(),
            target: self.target.as_ref().map(|t| kbf_invocation::Target {
                kind: t.kind,
                label: kbf_invocation::Text::sent(t.label.clone()),
            }),
            test: self.test,
        }
    }
}

/// The keys this process has described in an `action shape:` line.
#[derive(Debug, Default)]
pub struct Shapes {
    seen: Mutex<BTreeSet<ShapeKey>>,
}

impl Shapes {
    /// The subject of the action `described` names, its shape logged if its key is
    /// new. Consumes `described`: its argv and paths are not kept.
    pub fn subject(&self, described: Described) -> Subject {
        let (subject, shape) = Subject::of(described);
        if self.first_sight(shape.key) {
            tracing::info!("action shape: {}", shape_line(&shape));
        }
        subject
    }

    /// Whether `key` is new to this process; remembers it. Forgets every key once
    /// [`SEEN_KEYS`] are held.
    fn first_sight(&self, key: ShapeKey) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= SEEN_KEYS && !seen.contains(&key) {
            seen.clear();
        }
        seen.insert(key)
    }
}

/// The `action shape:` line's text after its prefix: the key, the program, how many
/// output patterns, and the first `SHOWN_PATTERNS` of them (`…` after them when
/// there are more).
#[must_use]
pub fn shape_line(shape: &Shape) -> String {
    let shown: Vec<&str> = shape
        .outputs
        .iter()
        .take(SHOWN_PATTERNS)
        .map(String::as_str)
        .collect();
    let more = if shape.outputs.len() > SHOWN_PATTERNS {
        "…"
    } else {
        ""
    };
    format!(
        "key={} program={:?} outputs={} patterns={:?}",
        shape.key,
        shape.program.join(" "),
        shape.outputs.len(),
        format!("{}{more}", shown.join(" ")),
    )
}

/// What a lease's report measured: its usage, and its exit code if it ended OK.
#[derive(Clone, Debug, PartialEq)]
pub struct Measured {
    usage: ResourceUsage,
    exit_code: Option<i32>,
}

impl Measured {
    /// What `result` measured; `None` for a result whose `action_result` carries no
    /// `ResourceUsage` (nothing was measured, so there is no line). Read before the
    /// result is judged: a memory kill's or a lost run's `action_result` is dropped
    /// there, and a timeout's cut to its logs.
    #[must_use]
    pub fn of(result: &worker::Result) -> Option<Self> {
        let action_result = result.action_result.as_ref()?;
        let usage = ResourceUsage::in_result(action_result)?;
        let ok = result
            .status
            .as_ref()
            .is_none_or(|s| s.code == Code::Ok as i32);
        Some(Self {
            usage,
            exit_code: ok.then_some(action_result.exit_code),
        })
    }

    /// The `ResourceUsage` the result carried.
    #[must_use]
    pub const fn usage(&self) -> &ResourceUsage {
        &self.usage
    }
}

/// The `outcome=` word of a report the scheduler judged `outcome`.
#[must_use]
pub const fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed { .. } => "ok",
        Outcome::Failed(failure) => match failure {
            Failure::Timeout => "timeout",
            Failure::Infra => "infra",
            Failure::Invalid => "invalid",
            Failure::OutOfMemory => "oom",
            Failure::NodeMemoryPressure => "node-pressure",
            Failure::MissingInput => "missing-input",
            Failure::Lost(LostWhy::DaemonRestarted) => "lost-restart",
            Failure::Lost(LostWhy::Fenced) => "lost-fenced",
        },
    }
}

/// The `action usage:` line's text after its prefix.
#[must_use]
pub fn line(
    subject: &Subject,
    measured: &Measured,
    judged: Outcome,
    lease: LeaseId,
    node: &WorkerId,
) -> String {
    let Measured { usage, exit_code } = measured;
    let text = |t: Option<&String>| t.map_or_else(|| "-".to_owned(), |t| format!("{t:?}"));
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
    let cgroup = usage
        .cgroup
        .as_ref()
        .map_or_else(kbf_proto::no_cgroup_fields, |c| {
            c.log_fields(usage.wall_micros)
        });
    let target = subject.target.as_ref();
    format!(
        "exit_code={} mnemonic={} target_hint={} wall_us={} {cgroup} key={} cfg={} target={} \
         target_kind={} outcome={} lease={lease} node={node}",
        dash(exit_code.map(|c| c.to_string())),
        text(subject.label.mnemonic.as_ref()),
        text(subject.label.target.as_ref()),
        usage.wall_micros,
        subject.key,
        dash(subject.cfg.clone()),
        text(target.map(|t| &t.label)),
        target.map_or("-", |t| t.kind.name()),
        outcome(judged),
    )
}

/// Logs the `action usage:` line of `operation`'s lease `lease` on `node`, whose
/// report `measured` what it used and which the scheduler judged `judged`.
pub fn log(
    operation: OperationId,
    subject: &Subject,
    measured: &Measured,
    judged: Outcome,
    lease: LeaseId,
    node: &WorkerId,
) {
    let line = line(subject, measured, judged, lease, node);
    tracing::info!(%operation, "action usage: {line}");
}

#[cfg(test)]
mod tests;
