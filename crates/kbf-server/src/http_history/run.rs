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
use kbf_history::records::{RunOutcome, RunRecord, Timestamps, WireDigest};
use kbf_invocation::WallTime;
use serde_json::{Map, Value, json};

use super::Durable;
use crate::history::run_record;
use crate::http_blobs::{error, refusal};
use crate::http_invocations::{InvocationReaders, json, pages_off, rfc3339};

/// The call name refusals are logged with.
const CALL: &str = "GET /v1/runs";

/// The routes' path prefix; the run id follows it.
const PREFIX: &str = "/v1/runs/";

struct RunState {
    recorder: Recorder,
    readers: Option<InvocationReaders>,
    durable: Arc<Durable>,
}

/// `GET /v1/runs/{id}` over `recorder`'s records and `durable`'s history, answering as
/// the build pages do: off unless the policy names an `invocations.getAuthorizer`.
pub(crate) fn router(
    recorder: Recorder,
    readers: Option<InvocationReaders>,
    durable: Arc<Durable>,
) -> Router {
    let state = Arc::new(RunState {
        recorder,
        readers,
        durable,
    });
    Router::new()
        .route("/v1/runs/{id}", get(page))
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
    let live = live(state, id);
    let record = if live.is_some() {
        live
    } else {
        state
            .durable
            .reads
            .run(id, &|_| true)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "build history could not be read");
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "build history cannot be read right now",
                )
            })?
            .map(|view| view.run)
    };
    let Some(record) = record else {
        let body = json!({"error": "no_run", "id": text}).to_string();
        let answer = (
            StatusCode::NOT_FOUND,
            [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
            body,
        );
        return Err(answer.into_response());
    };
    // The run's one instance, asked as a REAPI call's instance name is, before anything
    // of the run is answered.
    authorize(&*readers.authorizer, &caller, CALL, &record.instance)
        .await
        .map_err(|e| refusal(&e))?;
    Ok(render(&record))
}

/// The run id `text` is: 24 lowercase hex digits.
fn parse(text: &str) -> Option<RunId> {
    let id: RunId = text.parse().ok()?;
    (id.to_string() == text).then_some(id)
}

/// The run `id`, if the recorder still holds the row it was minted for and that row's
/// attempt still names it.
fn live(state: &RunState, id: RunId) -> Option<RunRecord> {
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
