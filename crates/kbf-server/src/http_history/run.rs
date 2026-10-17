//! `GET /v1/runs/{run_id}`: one attempt, from the recorder while it holds the record and
//! from history after, in one rendering (`docs/design/ui-data.md` 6.3). Both are
//! rendered from the run record the history writer keeps, so the answer is the same
//! before a restart and after it.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kbf_auth::authorize;
use kbf_front::Recorder;
use kbf_history::RunId;
use kbf_history::records::{
    CommandRendering, FailedRun, FlogStream, RunOutcome, RunRecord, Tail, TailOmitted, Timestamps,
    WireDigest,
};
use kbf_invocation::WallTime;
use kbf_types::Digest;
use serde_json::{Map, Value, json};

use super::logs;
use super::{Durable, Presence};
use crate::history::run_record;
use crate::http_blobs::{ReadBudget, error, refusal};
use crate::http_invocations::{InvocationReaders, blob_href, json, pages_off, rfc3339};

/// The call name refusals are logged with.
const CALL: &str = "GET /v1/runs";

/// The routes' path prefix; the run id follows it.
pub(super) const PREFIX: &str = "/v1/runs/";

pub(super) struct RunState {
    pub(super) recorder: Recorder,
    pub(super) readers: Option<InvocationReaders>,
    pub(super) durable: Arc<Durable>,
    /// Which blobs the cache still holds, for each log's `available` and `href`.
    pub(super) presence: Arc<dyn Presence>,
    /// The memory a log read holds, shared with the blob route and the tail workers.
    pub(super) budget: ReadBudget,
}

/// `GET /v1/runs/{id}` and its logs over `recorder`'s records and `durable`'s history,
/// answering as the build pages do: off unless the policy names an
/// `invocations.getAuthorizer`.
pub(crate) fn router(
    recorder: Recorder,
    readers: Option<InvocationReaders>,
    durable: Arc<Durable>,
    presence: Arc<dyn Presence>,
    budget: ReadBudget,
) -> Router {
    let state = Arc::new(RunState {
        recorder,
        readers,
        durable,
        presence,
        budget,
    });
    Router::new()
        .route("/v1/runs/{id}", get(page))
        .route("/v1/runs/{id}/logs/{stream}", get(logs::page))
        .with_state(state)
}

async fn page(State(state): State<Arc<RunState>>, parts: Parts) -> Response {
    let Some(readers) = &state.readers else {
        return pages_off();
    };
    match read(&state, readers, &parts).await {
        Ok(value) => json(&value),
        Err(answer) => answer,
    }
}

async fn read(
    state: &RunState,
    readers: &InvocationReaders,
    parts: &Parts,
) -> Result<Value, Response> {
    let caller = readers
        .authenticator
        .authenticate(parts)
        .await
        .map_err(|e| {
            tracing::warn!(call = CALL, error = %e.message(), "run read not authenticated");
            refusal(&e)
        })?;
    let text = parts.uri.path().strip_prefix(PREFIX).unwrap_or_default();
    let id = parse(text).ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            &format!("{text:?} is not a run id: 24 lowercase hex digits"),
        )
    })?;
    let Some((record, failed)) = find(state, id).await? else {
        return Err(no_run(text));
    };
    // The run's one instance, asked as a REAPI call's instance name is, before anything
    // of the run is answered.
    authorize(&*readers.authorizer, &caller, CALL, &record.instance)
        .await
        .map_err(|e| refusal(&e))?;
    let mut value = render(&record);
    add_logs(state, readers, &record, failed.as_ref(), &mut value).await;
    Ok(value)
}

/// The `404` of a run id nothing holds.
pub(super) fn no_run(text: &str) -> Response {
    let body = json!({"error": "no_run", "id": text}).to_string();
    let answer = (
        StatusCode::NOT_FOUND,
        [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
        body,
    );
    answer.into_response()
}

/// The run `id`, from the recorder while it holds it and from history after, with the
/// failed-run section history holds for a failed one (the recorder has none). `None` when
/// neither holds it.
///
/// # Errors
/// `503` when history cannot be read and the recorder does not hold the run.
pub(super) async fn find(
    state: &RunState,
    id: RunId,
) -> Result<Option<(RunRecord, Option<FailedRun>)>, Response> {
    let live = live(state, id);
    // A live run that did not fail has nothing more in history.
    if live.as_ref().is_some_and(|r| !r.failed()) {
        return Ok(live.map(|r| (r, None)));
    }
    let view = match state.durable.reads.run(id, &|_| true).await {
        Ok(view) => view,
        // The recorder's own record answers; only its tails are missing.
        Err(e) if live.is_some() => {
            tracing::warn!(error = %e, "build history could not be read");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "build history could not be read");
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "build history cannot be read right now",
            ));
        }
    };
    Ok(match (live, view) {
        (Some(record), view) => Some((record, view.and_then(|v| v.failed))),
        (None, Some(view)) => Some((view.run, view.failed)),
        (None, None) => None,
    })
}

/// The run id `text` is: 24 lowercase hex digits.
pub(super) fn parse(text: &str) -> Option<RunId> {
    let id: RunId = text.parse().ok()?;
    (id.to_string() == text).then_some(id)
}

/// The run `id`, if the recorder still holds the row it was minted for and that row's
/// attempt still names it.
pub(super) fn live(state: &RunState, id: RunId) -> Option<RunRecord> {
    let bytes = id.to_bytes();
    let (key, row) = state.durable.reads.runs().find(&bytes)?;
    let mut found = None;
    state.recorder.read(&key, &mut |inv| {
        let Some(row) = inv.rows().get(row) else {
            return;
        };
        found = row
            .attempts
            .iter()
            .find(|a| a.run_id == Some(bytes))
            .map(|a| {
                let mut record = run_record(row, a);
                record.set_run_id(id);
                record
            });
    });
    found
}

/// The digest `d` names, if it is a SHA-256 one.
fn digest_of(d: &WireDigest) -> Option<Digest> {
    let hash = <[u8; 32]>::try_from(d.hash.as_slice()).ok()?;
    Some(Digest::new(kbf_types::DigestFunction::Sha256, hash, d.size))
}

/// Adds `logs` (each log the run has: its digest, whether the blob is still held, a
/// link, and the tail history kept) and, for a failed run, its `command_rendering`.
async fn add_logs(
    state: &RunState,
    readers: &InvocationReaders,
    record: &RunRecord,
    failed: Option<&FailedRun>,
    value: &mut Value,
) {
    let wanted = [
        (FlogStream::Stdout, record.stdout.as_ref()),
        (FlogStream::Stderr, record.stderr.as_ref()),
        (FlogStream::Result, record.result.as_ref()),
    ];
    let logs: Vec<(FlogStream, &WireDigest, Option<Digest>)> = wanted
        .into_iter()
        .filter_map(|(stream, d)| Some((stream, d?, digest_of(d?))))
        .collect();
    let digests: Vec<Digest> = logs.iter().filter_map(|(_, _, d)| *d).collect();
    let present = state.presence.present(&digests).await;
    let mut held = present.iter().flatten().copied();
    let mut out = Map::new();
    let id = record.run_id();
    for (stream, wire, digest) in logs {
        let available = digest.and_then(|_| held.next());
        let blob = digest.map_or(Value::Null, |d| {
            blob_href(
                readers.blobs,
                &d,
                &record.instance,
                Some(stream.name()),
                false,
            )
        });
        // The blob while it is held; once it is not, the copy history kept, if it did.
        let href = if available == Some(false) || blob.is_null() {
            copy_href(state, id, stream, &record.instance)
                .await
                .unwrap_or(blob)
        } else {
            blob
        };
        let mut log = Map::new();
        log.insert("digest".to_owned(), digest_text(wire).into());
        log.insert(
            "available".to_owned(),
            available.map_or(Value::Null, Value::Bool),
        );
        log.insert("href".to_owned(), href);
        let tail = match stream {
            FlogStream::Stdout => failed.and_then(|f| f.stdout.as_ref()),
            FlogStream::Stderr => failed.and_then(|f| f.stderr.as_ref()),
            FlogStream::Result => None,
        };
        if let Some(tail) = tail {
            add_tail(&mut log, tail);
        }
        out.insert(stream.name().to_owned(), log.into());
    }
    // `render` made an object, and nothing since has changed that.
    let keys = value.as_object_mut().expect("a run renders as an object");
    if !out.is_empty() {
        keys.insert("logs".to_owned(), out.into());
    }
    if let Some(command) = failed.and_then(|f| f.command.as_ref()) {
        keys.insert("command_rendering".to_owned(), rendering(command));
    }
}

/// The failure-log route's link for `stream` of run `id`, if history holds a copy of it
/// for `instance` (the instance the caller was authorized on).
async fn copy_href(
    state: &RunState,
    id: RunId,
    stream: FlogStream,
    instance: &str,
) -> Option<Value> {
    let allow = |i: &str| i == instance;
    let held = state.durable.reads.failure_log(id, stream, &allow).await;
    // A copy that cannot be read now is not linked: the page does not fail for it.
    held.ok()
        .flatten()
        .map(|_| format!("{PREFIX}{id}/logs/{}", stream.name()).into())
}

/// A log's tail, as `tail` (text; invalid UTF-8 shown as the replacement character) with
/// its `total_bytes`, or `tail: null` and the reason it is missing. Not redacted: no
/// redaction machinery exists (ui-data.md Q8), and the key says so.
fn add_tail(log: &mut Map<String, Value>, tail: &Tail) {
    let why = match tail.omitted() {
        TailOmitted::No => None,
        TailOmitted::TooLarge => Some("too_large"),
        TailOmitted::Unreadable => Some("unreadable"),
        TailOmitted::Busy => Some("busy"),
    };
    let text = why.map_or_else(
        || Value::String(String::from_utf8_lossy(&tail.bytes).into_owned()),
        |_| Value::Null,
    );
    log.insert("tail".to_owned(), text);
    log.insert(
        "tail_omitted".to_owned(),
        why.map_or(Value::Null, Value::from),
    );
    log.insert("total_bytes".to_owned(), tail.total_bytes.into());
    log.insert("tail_redacted".to_owned(), false.into());
}

/// A command rendering as the page shows it: argv, environment names only, not redacted.
fn rendering(c: &CommandRendering) -> Value {
    json!({
        "argv": c.argv,
        "env_names": c.env_names,
        "working_directory": c.working_directory,
        "output_paths": c.output_paths,
        "truncated": c.truncated,
        "redacted": false,
    })
}

/// A digest as the API writes it.
fn digest_text(d: &WireDigest) -> String {
    format!("{}-{}", hex::encode(&d.hash), d.size)
}

/// One run, as `GET /v1/runs/{id}` answers it: what the run record keeps, in the shape
/// of an attempt (ui-data.md 6.3). A key the record has no value for is left out (a
/// memory booking of a run that was not killed, an exit code of a run that did not
/// exit). Not here yet: the `envelope`, `end`, the tails and the transfer counters.
pub(crate) fn render(r: &RunRecord) -> Value {
    let id = r.run_id().to_string();
    let t = r.timestamps.clone().unwrap_or_default();
    let mut keys = Map::new();
    let mut put = |key: &str, value: Value| {
        keys.insert(key.to_owned(), value);
    };
    put("run_id", id.clone().into());
    put("href", format!("{PREFIX}{id}").into());
    put("instance", r.instance.clone().into());
    put("lease", format!("{}.{}", r.lease_epoch, r.lease_seq).into());
    put("worker", r.node.clone().into());
    put(
        "queued",
        time(nonzero(r.attempt_queued_us).unwrap_or(t.queued_us)),
    );
    put("granted", time(t.granted_us));
    put("started", nonzero(r.started_us).map_or(Value::Null, time));
    put("ended", nonzero(r.ended_us).map_or(Value::Null, time));
    put("class", class(r.outcome()).into());
    if matches!(r.outcome(), RunOutcome::Ok | RunOutcome::Failed) {
        put("exit_code", r.exit_code.into());
    }
    if !r.reason.is_empty() {
        put("reason", r.reason.clone().into());
    }
    put("memory_kill", kill(r).into());
    for (key, bytes) in [
        ("booked_memory_bytes", r.booked_memory_bytes),
        ("cap_bytes", r.cap_bytes),
        ("raised_memory_bytes", r.raised_memory_bytes),
        ("memory_used_bytes", r.memory_used_bytes),
        ("memory_limit_bytes", r.memory_limit_bytes),
    ] {
        if bytes != 0 {
            put(key, bytes.into());
        }
    }
    if r.farm_budget != 0 {
        put("farm_runs", r.farm_runs.into());
        put("farm_budget", r.farm_budget.into());
    }
    for (key, text) in [("wait_reason", &r.wait_reason), ("partition", &r.partition)] {
        if !text.is_empty() {
            put(key, text.clone().into());
        }
    }
    if r.presubmit_us != 0 {
        put("presubmit_us", r.presubmit_us.into());
    }
    put("reapi_times", reapi_times(&t));
    if let Some(c) = &r.cgroup {
        put("usage", usage(c));
    }
    put("action", digest(r.action.as_ref()));
    put("command", digest(r.command.as_ref()));
    put("result", digest(r.result.as_ref()));
    put("stdout", digest(r.stdout.as_ref()));
    put("stderr", digest(r.stderr.as_ref()));
    put("shape", shape(r));
    Value::Object(keys)
}

fn nonzero(us: u64) -> Option<u64> {
    (us != 0).then_some(us)
}

/// A time in microseconds after the epoch, as the API writes one.
fn time(us: u64) -> Value {
    rfc3339(WallTime::from_unix_micros(us)).into()
}

/// A run's class, by the name the API gives it.
fn class(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Unspecified => "unspecified",
        RunOutcome::Ok => "ok",
        RunOutcome::Failed => "failed",
        RunOutcome::TimedOut => "timed_out",
        RunOutcome::MemoryKilled => "memory_killed",
        RunOutcome::Infra => "infra",
        RunOutcome::Lost => "lost",
        RunOutcome::Cancelled => "cancelled",
    }
}

/// The name of the memory kill that ended the run, as the build pages write it.
fn kill(r: &RunRecord) -> &'static str {
    use kbf_history::records::MemoryKill;
    match r.memory_kill() {
        MemoryKill::None => "none",
        MemoryKill::OwnLimit => "own_limit",
        MemoryKill::NodePressure => "node_pressure",
    }
}

/// The REAPI times the run reported, by their REAPI names; one not reported is left out.
fn reapi_times(t: &Timestamps) -> Value {
    [
        ("queued", t.queued_us),
        ("worker_start", t.worker_start_us),
        ("input_fetch_start", t.input_fetch_start_us),
        ("input_fetch_completed", t.input_fetch_completed_us),
        ("execution_start", t.execution_start_us),
        ("execution_completed", t.execution_completed_us),
        ("output_upload_start", t.output_upload_start_us),
        ("output_upload_completed", t.output_upload_completed_us),
        ("worker_completed", t.worker_completed_us),
    ]
    .into_iter()
    .filter_map(|(name, us)| Some((name.to_owned(), time(nonzero(us)?))))
    .collect::<Map<String, Value>>()
    .into()
}

/// The cgroup block of a run, with the parallelism derived from it.
fn usage(c: &kbf_history::records::Cgroup) -> Value {
    let mut keys = Map::new();
    for (key, value) in [
        ("wall_us", c.wall_us),
        ("cpu_usage_us", c.cpu_usage_us),
        ("cpu_user_us", c.cpu_user_us),
        ("cpu_system_us", c.cpu_system_us),
        ("cpu_periods", c.periods),
        ("cpu_throttled", c.throttled_periods),
        ("throttled_us", c.throttled_us),
        ("memory_peak_bytes", c.memory_peak_bytes),
    ] {
        keys.insert(key.to_owned(), value.into());
    }
    if c.wall_us > 0 {
        keys.insert(
            "parallelism".to_owned(),
            (c.cpu_usage_us as f64 / c.wall_us as f64).into(),
        );
    }
    let numa: Vec<Value> = c
        .numa_bytes
        .iter()
        .zip(0..)
        .filter(|(bytes, _)| **bytes != 0)
        .map(|(bytes, node): (&u64, u32)| json!({"node": node, "bytes": bytes}))
        .collect();
    if !numa.is_empty() {
        keys.insert("numa_bytes".to_owned(), numa.into());
    }
    keys.into()
}

/// A digest as the API writes it, `null` for none.
fn digest(d: Option<&WireDigest>) -> Value {
    d.map_or(Value::Null, |d| {
        format!("{}-{}", hex::encode(&d.hash), d.size).into()
    })
}

/// The run's shape: the key, the configuration hash, the target and whether the action
/// is a test; `null` for a run with none.
fn shape(r: &RunRecord) -> Value {
    if r.shape == 0 && r.cfg == "-" && r.target.is_empty() && !r.test {
        return Value::Null;
    }
    let kind = match r.target_kind {
        1 => "target",
        2 => "anon",
        3 => "bxl",
        _ => "",
    };
    let target = if r.target.is_empty() || kind.is_empty() {
        Value::Null
    } else {
        json!({"label": r.target, "kind": kind})
    };
    json!({"key": format!("{:016x}", r.shape), "cfg": r.cfg, "target": target, "test": r.test})
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
