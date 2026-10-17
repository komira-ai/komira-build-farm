//! The JSON the build pages answer, built from one record (`docs/api.md`,
//! `docs/design/invocations.md` sections 4.2 and 4.4), and the blobs it lists, whose
//! `available` is filled in once the cache has been asked.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use kbf_invocation::{
    ActionRow, ActionShape, Attempt, AttemptEnd, Counts, Durations, Histogram, How, Invocation,
    Outcome, Phase, Run, RunUsage, State, Text, Usage, WallTime, failure_name, requeue_name,
};
use kbf_sched::RequeueReason;
use kbf_types::Digest;
use serde_json::{Map, Value, json};

use super::pages::{failed, how_name};
use crate::http_blobs::INLINE_MAX_BYTES;

/// A route's JSON before the blobs it lists are looked up: where each blob's object
/// sits in it (a JSON pointer), whose `available` is to be set.
#[derive(Debug, Default)]
pub(crate) struct Draft {
    /// The answer, with `available: null` in each blob's object.
    pub(crate) value: Value,
    /// Each blob's object, as a JSON pointer into `value`, and its digest.
    pub(crate) blobs: Vec<(String, Digest)>,
}

impl Draft {
    /// Sets each blob's `available` to the answer `present` gives for it, in order.
    pub(crate) fn fill(mut self, present: &[bool]) -> Value {
        for ((at, _), &held) in self.blobs.iter().zip(present) {
            let available = self
                .value
                .pointer_mut(&format!("{at}/available"))
                .expect("every blob noted holds an `available`");
            *available = Value::Bool(held);
        }
        self.value
    }
}

/// Builds a route's answer, noting each blob it lists.
pub(super) struct Builder {
    /// Whether the blob route answers: without it every `href` is `null`.
    pub(super) blobs_on: bool,
    pub(super) found: Vec<(String, Digest)>,
}

impl Builder {
    /// A blob's object, `{"digest", "available", "href"}`, noted under the pointer
    /// `at`; `href` reads it on `instance`, named `name`, as text when `inline`.
    fn blob(
        &mut self,
        at: String,
        digest: Digest,
        instance: &str,
        name: Option<&str>,
        inline: bool,
    ) -> Map<String, Value> {
        self.found.push((at, digest));
        let mut blob = Map::new();
        blob.insert("digest".to_owned(), digest_text(&digest).into());
        blob.insert("available".to_owned(), Value::Null);
        blob.insert(
            "href".to_owned(),
            self.href(&digest, instance, name, inline),
        );
        blob
    }

    /// `/v1/blobs/{digest}?instance=..` for a blob, or `null` with the blob route off.
    fn href(&self, digest: &Digest, instance: &str, name: Option<&str>, inline: bool) -> Value {
        if !self.blobs_on {
            return Value::Null;
        }
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("instance", instance);
        if let Some(name) = name {
            query.append_pair("name", name);
        }
        if inline {
            query.append_pair("inline", "text");
        }
        format!("/v1/blobs/{}?{}", digest_text(digest), query.finish()).into()
    }

    /// A log's object, linked as text when it is small enough to be shown inline.
    fn log(&mut self, at: String, digest: Option<Digest>, instance: &str, name: &str) -> Value {
        digest.map_or(Value::Null, |d| {
            let inline = d.size_bytes <= INLINE_MAX_BYTES;
            Value::Object(self.blob(at, d, instance, Some(name), inline))
        })
    }
}

/// The invocation's own answer (section 4.2).
pub(super) fn invocation(inv: &Invocation, base: &str, now: WallTime) -> Value {
    let (tool, tool_version) = inv.tool();
    let collisions: Vec<String> = inv.collisions().iter().map(ToString::to_string).collect();
    let state = match inv.state(now) {
        State::Open => "open",
        State::Idle => "idle",
    };
    let summary = Summary::of(inv);
    json!({
        "id": inv.key().to_string(),
        "url": Value::Null,
        "tool": tool,
        "tool_version": tool_version,
        "correlated_invocations_id": inv.correlated_invocations_id(),
        "instances": inv.instances(),
        "first_seen": rfc3339(inv.first_seen()),
        "last_seen": rfc3339(inv.last_seen()),
        "state": state,
        "complete": inv.complete(),
        "rows_capped": inv.rows_capped(),
        "actions": inv.rows().len(),
        "counts": counts(inv.counts()),
        "collisions": collisions,
        "links": {
            "actions": format!("{base}/actions"),
            "artifacts": format!("{base}/artifacts"),
            "logs": format!("{base}/logs"),
            "metrics": format!("{base}/metrics"),
        },
        "phase_totals": summary.totals(),
        "top_slowest": {
            "wall_ms": summary.wall.list(inv, Phase::Wall, base),
            "execution_ms": summary.execution.list(inv, Phase::Execution, base),
        },
    })
}

/// How many actions `top_slowest` lists per phase.
pub(crate) const TOP_SLOWEST: usize = 20;

/// The phases `phase_totals` sums: every one but wall, whose sum over rows that ran
/// side by side means nothing.
const TOTALLED: [Phase; 4] = [
    Phase::Queue,
    Phase::Execution,
    Phase::InputFetch,
    Phase::OutputUpload,
];

/// What the summary reads from the rows the farm ran for this invocation (`how` =
/// `ran`; a joined row's leases are its twin's, counted where the twin ran), in one
/// pass with each row's durations derived once: it runs under the records' lock.
struct Summary {
    /// Per phase of [`TOTALLED`]: its microseconds summed over the rows that measured
    /// it, and how many did.
    totals: [(u64, u64); 4],
    wall: Slowest,
    execution: Slowest,
}

impl Summary {
    fn of(inv: &Invocation) -> Self {
        let mut summary = Self {
            totals: [(0, 0); 4],
            wall: Slowest::default(),
            execution: Slowest::default(),
        };
        for (i, r) in inv.rows().iter().enumerate() {
            if r.how != How::Ran {
                continue;
            }
            let d = r.durations();
            for (total, phase) in summary.totals.iter_mut().zip(TOTALLED) {
                if let Some(micros) = d.of(phase) {
                    *total = (total.0.saturating_add(micros), total.1 + 1);
                }
            }
            summary.wall.offer(d.wall, i);
            summary.execution.offer(d.execution, i);
        }
        summary
    }

    /// `phase_totals`: per phase some row measured, its milliseconds summed over the
    /// rows that measured it (`sum`) and how many did (`rows`); a phase none measured
    /// is left out.
    fn totals(&self) -> Value {
        let mut totals = Map::new();
        for (&(sum, rows), phase) in self.totals.iter().zip(TOTALLED) {
            if rows > 0 {
                totals.insert(
                    phase.name().to_owned(),
                    json!({"sum": sum / 1_000, "rows": rows}),
                );
            }
        }
        Value::Object(totals)
    }
}

/// The [`TOP_SLOWEST`] rows that took longest in one phase, kept while the rows are
/// read: a heap whose top is the one to drop next (the shortest, the later row of a
/// tie).
#[derive(Default)]
struct Slowest(BinaryHeap<(Reverse<u64>, usize)>);

impl Slowest {
    /// Row `i`, which took `micros` (if it measured the phase).
    fn offer(&mut self, micros: Option<u64>, i: usize) {
        let Some(micros) = micros else {
            return;
        };
        self.0.push((Reverse(micros), i));
        if self.0.len() > TOP_SLOWEST {
            self.0.pop();
        }
    }

    /// The rows kept, longest first (ties in row order): each one's action, its
    /// milliseconds in `phase` and its `href`.
    fn list(&self, inv: &Invocation, phase: Phase, base: &str) -> Value {
        let mut kept: Vec<(Reverse<u64>, usize)> = self.0.iter().copied().collect();
        kept.sort_unstable();
        kept.into_iter()
            .map(|(Reverse(micros), i)| {
                let action = digest_text(&inv.rows()[i].action);
                json!({
                    "action": action,
                    phase.name(): micros / 1_000,
                    "href": format!("{base}/actions/{action}"),
                })
            })
            .collect()
    }
}

fn counts(c: &Counts) -> Value {
    json!({
        "execute": c.execute,
        "cache_hit_execute": c.cache_hit_execute,
        "joined": c.joined,
        "ran": c.ran,
        "failed": c.failed,
        "ac_hit": c.ac_hit,
        "ac_miss": c.ac_miss,
        "blobs_asked": c.blobs_asked,
        "blobs_missing": c.blobs_missing,
        "bytes_uploaded": c.bytes_uploaded,
        "bytes_downloaded": c.bytes_downloaded,
    })
}

/// A row as a page of `/actions` lists it, with `href` to its own answer.
pub(super) fn row(r: &ActionRow, base: &str) -> Map<String, Value> {
    let attempts: Vec<Value> = r.attempts.iter().map(attempt).collect();
    let action = digest_text(&r.action);
    let href = format!("{base}/actions/{action}");
    [
        ("action", action.into()),
        ("instance", r.instance.clone().into()),
        ("target", text(r.target.as_ref())),
        ("mnemonic", text(r.mnemonic.as_ref())),
        ("configuration", text(r.configuration.as_ref())),
        ("action_id", text(r.action_id.as_ref())),
        ("how", how_name(r.how).into()),
        ("operation", r.operation.clone().into()),
        ("attempts", attempts.into()),
        ("outcome", outcome(r.outcome.as_ref())),
        (
            "result",
            r.listing
                .as_ref()
                .and_then(|l| l.result)
                .as_ref()
                .map(digest_text)
                .into(),
        ),
        ("command", r.command.as_ref().map(digest_text).into()),
        ("href", href.into()),
        ("shape", r.shape.as_deref().map_or(Value::Null, shape)),
        ("durations", durations(&r.durations())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// One action's answer: its row, with the listing copied into it (section 4.2).
pub(super) fn action(b: &mut Builder, r: &ActionRow, base: &str) -> Map<String, Value> {
    let mut map = row(r, base);
    let listing = r.listing.as_ref();
    let outputs: Vec<Value> = listing
        .map(|l| l.outputs.as_slice())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let name = (!o.directory).then(|| file_name(&o.path));
            let mut blob = b.blob(format!("/outputs/{i}"), o.digest, &r.instance, name, false);
            blob.insert("path".to_owned(), o.path.clone().into());
            blob.insert("executable".to_owned(), o.executable.into());
            blob.insert("directory".to_owned(), o.directory.into());
            Value::Object(blob)
        })
        .collect();
    let stdout = b.log(
        "/stdout".to_owned(),
        listing.and_then(|l| l.stdout),
        &r.instance,
        "stdout",
    );
    let stderr = b.log(
        "/stderr".to_owned(),
        listing.and_then(|l| l.stderr),
        &r.instance,
        "stderr",
    );
    let command = r
        .command
        .as_ref()
        .map_or(Value::Null, |d| b.href(d, &r.instance, None, false));
    map.insert("outputs".to_owned(), Value::Array(outputs));
    map.insert("outputs_truncated".to_owned(), r.outputs_truncated.into());
    map.insert("stdout".to_owned(), stdout);
    map.insert("stderr".to_owned(), stderr);
    map.insert("usage".to_owned(), usage(listing.and_then(|l| l.usage)));
    let blobs = json!({
        "action": b.href(&r.action, &r.instance, None, false),
        "command": command,
    });
    map.insert("blobs".to_owned(), blobs);
    map
}

/// One output of row `r` as `/artifacts` lists it.
pub(super) fn artifact(b: &mut Builder, r: &ActionRow, output: usize, at: &str) -> Value {
    let o = &r
        .listing
        .as_ref()
        .expect("a cursor names outputs that exist")
        .outputs[output];
    let name = (!o.directory).then(|| file_name(&o.path));
    let mut blob = b.blob(at.to_owned(), o.digest, &r.instance, name, false);
    blob.insert("action".to_owned(), digest_text(&r.action).into());
    blob.insert("path".to_owned(), o.path.clone().into());
    blob.insert("executable".to_owned(), o.executable.into());
    blob.insert("directory".to_owned(), o.directory.into());
    Value::Object(blob)
}

/// Row `r`'s logs as `/logs` lists them.
pub(super) fn logs(b: &mut Builder, r: &ActionRow, base: &str, at: &str) -> Value {
    let listing = r.listing.as_ref();
    let stdout = b.log(
        format!("{at}/stdout"),
        listing.and_then(|l| l.stdout),
        &r.instance,
        "stdout",
    );
    let stderr = b.log(
        format!("{at}/stderr"),
        listing.and_then(|l| l.stderr),
        &r.instance,
        "stderr",
    );
    let action = digest_text(&r.action);
    json!({
        "action": action,
        "href": format!("{base}/actions/{action}"),
        "failed": failed(r),
        "outcome": outcome(r.outcome.as_ref()),
        "stdout": stdout,
        "stderr": stderr,
    })
}

/// The invocation's aggregates (section 4.4).
pub(super) fn metrics(inv: &Invocation) -> Value {
    let m = inv.metrics();
    let usage = m.usage_actions > 0;
    let sum_max = |sum: u64, max: u64| {
        if usage {
            json!({"sum": sum, "max": max})
        } else {
            Value::Null
        }
    };
    json!({
        "counts": counts(inv.counts()),
        "queue_micros": histogram(&m.queue),
        "run_micros": histogram(&m.run),
        "reruns": m.reruns,
        "usage_actions": m.usage_actions,
        "cpu_micros": sum_max(m.cpu_micros_sum, m.cpu_micros_max),
        "peak_memory_bytes": sum_max(m.peak_memory_sum, m.peak_memory_max),
    })
}

fn histogram(h: &Histogram) -> Value {
    json!({
        "count": h.count(),
        "min": h.min(),
        "p50": h.percentile(50),
        "p90": h.percentile(90),
        "max": h.max(),
        "sum": h.sum(),
    })
}

/// A row's shape: the key, the configuration hash and the target the server read
/// from the action's Command (`target` beside it on the row is the client's hint).
fn shape(s: &ActionShape) -> Value {
    let target = s.target.as_ref().map_or(
        Value::Null,
        |t| json!({"label": t.label.value, "truncated": t.label.truncated, "kind": t.kind.name()}),
    );
    json!({"key": s.key_text(), "cfg": s.cfg, "target": target})
}

/// A row's durations, in milliseconds, under their phase names; one not measured is
/// left out.
fn durations(d: &Durations) -> Value {
    Phase::NAMED
        .into_iter()
        .filter_map(|(phase, name)| Some((name.to_owned(), (d.of(phase)? / 1_000).into())))
        .collect::<Map<String, Value>>()
        .into()
}

/// What an attempt's lease measured, as keys added to its built ones: its memory
/// kill once it ended, the booking and cap of a memory kill, the raised booking of an
/// own-limit requeue, the REAPI times and the usage of its run. A key not measured is
/// left out.
fn measured(a: &Attempt, keys: &mut Map<String, Value>) {
    let mut put = |key: &str, value: Value| {
        keys.insert(key.to_owned(), value);
    };
    if let Some(kill) = a.memory_kill() {
        put("memory_kill", kill.name().into());
    }
    let run = a.run.as_deref();
    let (booked, raised) = match &a.end {
        Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory { booked, raised })) => {
            (Some(*booked), Some(*raised))
        }
        _ => (run.and_then(|r| r.booked_memory_bytes), None),
    };
    for (key, bytes) in [
        ("booked_memory_bytes", booked),
        ("cap_bytes", run.and_then(|r| r.cap_bytes)),
        ("raised_memory_bytes", raised),
    ] {
        if let Some(bytes) = bytes {
            put(key, bytes.into());
        }
    }
    if let Some(run) = run {
        put("reapi_times", reapi_times(run));
        if let Some(u) = &run.usage {
            put("usage", run_usage(u));
        }
    }
}

fn reapi_times(run: &Run) -> Value {
    run.times
        .named()
        .into_iter()
        .filter_map(|(name, t)| Some((name.to_owned(), rfc3339(t?).into())))
        .collect::<Map<String, Value>>()
        .into()
}

/// A run's usage: its wall time, every cgroup counter it reported, the parallelism
/// derived from them and its bytes per NUMA node.
fn run_usage(u: &RunUsage) -> Value {
    let mut keys = Map::new();
    keys.insert("wall_us".to_owned(), u.wall_micros.into());
    for (key, value) in [
        ("cpu_usage_us", u.cpu_usage_micros),
        ("cpu_user_us", u.cpu_user_micros),
        ("cpu_system_us", u.cpu_system_micros),
        ("cpu_periods", u.cpu_periods),
        ("cpu_throttled", u.cpu_throttled_periods),
        ("throttled_us", u.cpu_throttled_micros),
        ("memory_peak_bytes", u.memory_peak_bytes),
    ] {
        if let Some(value) = value {
            keys.insert(key.to_owned(), value.into());
        }
    }
    if let Some(parallelism) = u.parallelism() {
        keys.insert("parallelism".to_owned(), parallelism.into());
    }
    if !u.numa.is_empty() {
        let numa: Vec<Value> = u
            .numa
            .iter()
            .map(|n| json!({"node": n.node, "bytes": n.bytes}))
            .collect();
        keys.insert("numa_bytes".to_owned(), numa.into());
    }
    keys.into()
}

fn attempt(a: &Attempt) -> Value {
    let end = match &a.end {
        None => Value::Null,
        Some(AttemptEnd::Accepted) => json!({"result": "ok"}),
        Some(AttemptEnd::Requeued(reason)) => {
            json!({"requeue": requeue_name(reason), "reason": reason.to_string()})
        }
        Some(AttemptEnd::Failed(failure, why)) => {
            json!({"failure": failure_name(*failure), "reason": why})
        }
    };
    let mut keys: Map<String, Value> = [
        ("lease", a.lease.to_string().into()),
        ("worker", a.worker.as_str().into()),
        ("queued", rfc3339(a.queued).into()),
        ("granted", rfc3339(a.granted).into()),
        ("started", a.started.map(rfc3339).into()),
        ("ended", a.ended.map(rfc3339).into()),
        ("end", end),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    measured(a, &mut keys);
    Value::Object(keys)
}

fn outcome(o: Option<&Outcome>) -> Value {
    match o {
        None => Value::Null,
        Some(Outcome::Ok { exit_code }) => json!({"ok": {"exit_code": exit_code}}),
        Some(Outcome::Failed { code, message }) => {
            json!({"failed": {"code": code, "message": message}})
        }
        Some(Outcome::TimedOut) => json!({"timed_out": {}}),
        Some(Outcome::Refused(why)) => json!({"refused": {"reason": why}}),
    }
}

fn text(t: Option<&Text>) -> Value {
    t.map_or(
        Value::Null,
        |t| json!({"value": t.value, "truncated": t.truncated}),
    )
}

fn usage(u: Option<Usage>) -> Value {
    u.map_or(Value::Null, |u| {
        json!({
            "cpu_user_micros": u.cpu_user_micros,
            "cpu_system_micros": u.cpu_system_micros,
            "peak_memory_bytes": u.peak_memory_bytes,
            "wall_micros": u.wall_micros,
        })
    })
}

/// The last component of an output's path: the file name its link downloads as.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A digest as the operator API writes it: `<sha256 hex>-<size>`.
pub(crate) fn digest_text(d: &Digest) -> String {
    format!("{}-{}", d.hash_hex(), d.size_bytes)
}

/// A wall time as RFC 3339 in UTC, to the microsecond:
/// `2026-10-11T09:00:00.000000Z`.
pub(crate) fn rfc3339(t: WallTime) -> String {
    let micros = t.unix_micros();
    let (secs, frac) = (micros / 1_000_000, micros % 1_000_000);
    let (days, day) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil(days);
    let (h, min, s) = (day / 3_600, day % 3_600 / 60, day % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{frac:06}Z")
}

/// The year, month and day of `days` days after 1970-01-01, in the proleptic Gregorian
/// calendar (H. Hinnant's `civil_from_days`, for days that are never negative).
const fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}
