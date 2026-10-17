//! The invocation record end to end (docs/design/invocations.md, section 9, PR 3): a
//! REAPI client that sends a buck2-shaped `RequestMetadata` header on every call, the
//! server with its recorder, and a fake daemon on the worker stream. The rows a build
//! gets for a cache hit, a run, a requeue and a failure are read back from the
//! server's recorder.
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic,
//! and coverage counts each generic function by its best-covered instantiation.

use std::io::Write as _;

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_invocation::{AttemptEnd, How, Invocation, InvocationId, Outcome, RecordKey};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    ActionResult, BatchReadBlobsRequest, BatchUpdateBlobsRequest, ExecuteRequest,
    FindMissingBlobsRequest, GetActionResultRequest, RequestMetadata, ToolDetails,
    action_cache_client::ActionCacheClient, batch_update_blobs_request,
};
use kbf_proto::worker;
use kbf_sched::RequeueReason;
use kbf_types::{Failure, LostWhy};
use prost::Message;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::{Request, Streaming};

use crate::support::{
    Blob, Cell, Job, done, failed, output, ran, report_every_run, response, stamped_response,
};

/// The build's id, as buck2 prints it (`Build ID:`).
pub(crate) const BUILD: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

/// `message` with the `RequestMetadata` header buck2 sends, naming invocation `id`.
pub(crate) fn tagged<T>(id: &str, message: T) -> Request<T> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: id.to_owned(),
        ..Default::default()
    };
    let mut request = Request::new(message);
    request.metadata_mut().insert_bin(
        REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&metadata.encode_to_vec()),
    );
    request
}

pub(crate) async fn execute(cell: &Cell, id: &str, action: &Blob) -> Streaming<Operation> {
    let request = ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(action.proto.clone()),
        ..Default::default()
    };
    ExecutionClient::new(cell.channel())
        .execute(tagged(id, request))
        .await
        .expect("Execute")
        .into_inner()
}

async fn get_action_result(cell: &Cell, id: &str, action: &Blob) -> Result<ActionResult, Code> {
    let request = GetActionResultRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(action.proto.clone()),
        ..Default::default()
    };
    ActionCacheClient::new(cell.channel())
        .get_action_result(tagged(id, request))
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| s.code())
}

/// Uploads `blobs` in one BatchUpdateBlobs call naming `id`; their bytes.
pub(crate) async fn upload(cell: &Cell, id: &str, blobs: &[&Blob]) -> u64 {
    let request = BatchUpdateBlobsRequest {
        instance_name: "main".to_owned(),
        requests: blobs
            .iter()
            .map(|b| batch_update_blobs_request::Request {
                digest: Some(b.proto.clone()),
                data: b.data.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    ContentAddressableStorageClient::new(cell.channel())
        .batch_update_blobs(tagged(id, request))
        .await
        .expect("BatchUpdateBlobs");
    blobs.iter().map(|b| b.data.len() as u64).sum()
}

/// The record of `id`, once the recorder has applied every event so far.
pub(crate) async fn record(cell: &Cell, id: &str) -> Invocation {
    cell.recorder.caught_up().await;
    cell.recorder
        .get(&RecordKey::owner(InvocationId::new(id)))
        .unwrap_or_else(|| panic!("no record of {id}"))
}

/// The digest of the ActionResult blob the server stored for `result`.
fn stored(result: &ActionResult) -> kbf_types::Digest {
    kbf_segments::sha256(&result.encode_to_vec())
}

/// Catches each seam of section 3.4 not wired, or wired out of order:
/// - a GetActionResult hit with no row, or its listing not the stored result's
///   (plant "no listing on a hit": the hit row's outputs go red);
/// - the Execute call sent after its request is queued (plant: `record_submit` after
///   `Event::Submit`), so the lease events reach no row: the run's attempts go red;
/// - a requeue not sent from the drain (plant: no `record_requeue`): the requeued
///   action has one attempt, or its first has no end;
/// - the daemon's INFRA reason dropped (plant `Detail::Infra` with an empty text): the
///   failed attempt's reason goes red, while the client still sees only the farm's
///   words;
/// - the outcome of the failure recorded as OK, and counts that miss a call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_records_a_hit_a_run_a_requeue_and_a_failure() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;

    // An action built before, by a call without an id: not recorded, but cached.
    let hit = Job::new("built before", &[]);
    cell.upload(&hit.blobs()).await;
    let mut ops = cell.execute(&hit.action).await;
    let start = daemon.start().await;
    let built = output(&cell, "built before", 0).await;
    assert!(daemon.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;

    // The build: a hit, a miss, then three actions to run.
    let cached = get_action_result(&cell, BUILD, &hit.action)
        .await
        .expect("a hit");
    let missing = Job::new("never built", &[]);
    let miss = get_action_result(&cell, BUILD, &missing.action).await;
    assert_eq!(miss, Err(Code::NotFound));

    let run = Job::new("runs once", &[]);
    let requeue = Job::new("runs twice", &[]);
    let fail = Job::new("fails", &[]);
    let mut uploaded = 0;
    for job in [&run, &requeue, &fail] {
        uploaded += upload(&cell, BUILD, &job.blobs()).await;
    }

    let mut ops = execute(&cell, BUILD, &run.action).await;
    let start = daemon.start().await;
    let run_result = output(&cell, "ran", 0).await;
    assert!(
        daemon
            .report(ran(start.lease_id, &run_result))
            .await
            .accepted
    );
    let run_done = done(&mut ops).await;

    let mut ops = execute(&cell, BUILD, &requeue.action).await;
    let first = daemon.start().await;
    let lost = worker::Result {
        lost: worker::Lost::Fenced as i32,
        ..failed(first.lease_id, Code::Unavailable)
    };
    assert!(daemon.report(lost).await.accepted);
    let second = daemon.start().await;
    let requeue_result = output(&cell, "ran again", 0).await;
    assert!(
        daemon
            .report(ran(second.lease_id, &requeue_result))
            .await
            .accepted
    );
    done(&mut ops).await;

    let mut ops = execute(&cell, BUILD, &fail.action).await;
    let start = daemon.start().await;
    let why = "image docker://registry.test/base@sha256:00 is not in this node's image store";
    // The farm's failure is run again; the last run's report finishes the operation.
    report_every_run(&mut daemon, start, |lease| {
        let mut report = failed(lease, Code::Internal);
        report.status.as_mut().expect("a status").message = why.to_owned();
        report
    })
    .await;
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert!(
        status.message.starts_with("kbf farm fault on node-a:"),
        "{status:?}"
    );

    let inv = record(&cell, BUILD).await;
    assert!(inv.complete(), "no event was dropped");
    assert_eq!(inv.tool(), (Some("buck2"), Some("0.1.0")));
    let actions: Vec<_> = inv.rows().iter().map(|r| (r.action, r.how)).collect();
    assert_eq!(
        actions,
        [
            (hit.action.digest, How::CacheHit),
            (run.action.digest, How::Ran),
            (requeue.action.digest, How::Ran),
            (fail.action.digest, How::Ran),
        ],
        "one row per action, in the order first seen; the miss has none"
    );
    let [hit_row, run_row, requeue_row, fail_row] = inv.rows() else {
        unreachable!("four rows, checked above");
    };

    // The hit copies the stored result's listing.
    let listing = hit_row.listing.as_ref().expect("the hit's listing");
    assert_eq!(listing.result, Some(stored(&cached)));
    let outputs: Vec<_> = listing.outputs.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(outputs, ["out"]);
    assert_eq!(hit_row.outcome, Some(Outcome::Ok { exit_code: 0 }));

    // The run: one lease on node-a, accepted; the result the client was given.
    assert_eq!(run_row.operation.as_deref(), Some(run_done.name.as_str()));
    let [attempt] = run_row.attempts.as_slice() else {
        panic!("one attempt: {:?}", run_row.attempts);
    };
    assert_eq!(attempt.worker.as_str(), "node-a");
    assert!(attempt.started.is_some(), "its Start was sent");
    assert_eq!(attempt.end, Some(AttemptEnd::Accepted));
    assert_eq!(run_row.outcome, Some(Outcome::Ok { exit_code: 0 }));
    let given = stamped_response(&run_done).result.expect("a result");
    let listing = run_row.listing.as_ref().expect("the run's listing");
    assert_eq!(listing.result, Some(stored(&given)));
    assert_eq!(listing.outputs[0].digest.size_bytes, "ran".len() as u64);

    // The requeue: the lost lease given up, then the rerun accepted.
    let ends: Vec<_> = requeue_row.attempts.iter().map(|a| a.end.clone()).collect();
    assert_eq!(
        ends,
        [
            Some(AttemptEnd::Requeued(RequeueReason::Lost(LostWhy::Fenced))),
            Some(AttemptEnd::Accepted),
        ]
    );
    let (lost, rerun) = (&requeue_row.attempts[0], &requeue_row.attempts[1]);
    assert_ne!(lost.lease, rerun.lease);
    assert_eq!(Some(rerun.queued), lost.ended, "queued again when given up");

    // The failure: the client's status, and the daemon's reason on the attempt.
    assert_eq!(
        fail_row.outcome,
        Some(Outcome::Failed {
            code: Code::Internal as i32,
            message: "kbf farm fault on node-a: the farm could not run the action (infra \
                      failure), and its reruns are used up. Operator fix: read the node \
                      daemon's log for why its driver failed. Runs: 3 (node-a, infra \
                      failure; node-a, infra failure; node-a, infra failure)."
                .to_owned(),
        })
    );
    let ends: Vec<_> = fail_row.attempts.iter().map(|a| a.end.clone()).collect();
    let again = Some(AttemptEnd::Requeued(RequeueReason::Infra));
    assert_eq!(
        ends[..2],
        [again.clone(), again],
        "the first two runs were run again"
    );
    let end = fail_row.attempts.last().and_then(|a| a.end.clone());
    assert_eq!(
        end,
        Some(AttemptEnd::Failed(Failure::Infra, why.to_owned()))
    );

    let c = inv.counts();
    assert_eq!(
        (c.execute, c.ran, c.joined, c.cache_hit_execute, c.failed),
        (3, 3, 0, 0, 1)
    );
    assert_eq!((c.ac_hit, c.ac_miss), (1, 1));
    assert_eq!(c.bytes_uploaded, uploaded);

    // What the stream costs per action in the stored encoding (section 3.3).
    let chunk = inv.chunk(0).encoded_len();
    let header = inv.header().encoded_len();
    let rows = inv.rows().len();
    let _ = writeln!(
        std::io::stderr(),
        "invocation record: {rows} rows, chunk {chunk} B ({} B per row), header {header} B",
        chunk / rows
    );
}

/// Catches a joined twin recorded as run, or without its twin's attempts; a cached
/// Execute not recorded as `cache_hit_execute`; and the CAS and ByteStream calls not
/// counted (FindMissingBlobs asked and missing, bytes read and written), each on the
/// record of the id it carried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joins_cached_executes_and_blob_calls_are_recorded() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("shared", &[]);
    cell.upload(&job.blobs()).await;

    let mut first = execute(&cell, BUILD, &job.action).await;
    let start = daemon.start().await;
    let mut twin = execute(&cell, "second-build", &job.action).await;
    let result = output(&cell, "shared output", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut first).await;
    done(&mut twin).await;
    let joined = record(&cell, "second-build").await;
    let [row] = joined.rows() else {
        panic!("one row: {:?}", joined.rows());
    };
    assert_eq!(row.how, How::Joined);
    let ends: Vec<_> = row.attempts.iter().map(|a| a.end.clone()).collect();
    assert_eq!(ends, [Some(AttemptEnd::Accepted)], "the twin's attempt");
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 0 }));

    let mut again = execute(&cell, "second-build", &job.action).await;
    done(&mut again).await;
    let second = record(&cell, "second-build").await;
    let [row] = second.rows() else {
        panic!("one row: {:?}", second.rows());
    };
    assert_eq!(row.how, How::CacheHitExecute, "the last Execute shows");
    let c = second.counts();
    assert_eq!((c.execute, c.joined, c.cache_hit_execute), (2, 1, 1));
    let first_build = record(&cell, BUILD).await;
    assert_eq!(first_build.rows()[0].attempts.len(), 1);

    // The blob calls, under a third id.
    let id = "blob-calls";
    let blob = Blob::new("written over ByteStream");
    let absent = Blob::new("never uploaded");
    let find = FindMissingBlobsRequest {
        instance_name: "main".to_owned(),
        blob_digests: vec![job.input.proto.clone(), absent.proto.clone()],
        ..Default::default()
    };
    let mut cas = ContentAddressableStorageClient::new(cell.channel());
    cas.find_missing_blobs(tagged(id, find))
        .await
        .expect("FindMissingBlobs");
    let read = BatchReadBlobsRequest {
        instance_name: "main".to_owned(),
        digests: vec![job.input.proto.clone()],
        ..Default::default()
    };
    cas.batch_read_blobs(tagged(id, read))
        .await
        .expect("BatchReadBlobs");
    let resource = format!(
        "main/uploads/0f0e0d0c-0000-4000-8000-000000000001/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    );
    let write = WriteRequest {
        resource_name: resource,
        write_offset: 0,
        finish_write: true,
        data: blob.data.clone(),
    };
    let mut bytestream = ByteStreamClient::new(cell.channel());
    bytestream
        .write(tagged(id, futures::stream::iter([write])))
        .await
        .expect("Write");
    let read = ReadRequest {
        resource_name: format!(
            "main/blobs/{}/{}",
            blob.digest.hash_hex(),
            blob.digest.size_bytes
        ),
        ..Default::default()
    };
    let mut stream = bytestream
        .read(tagged(id, read))
        .await
        .expect("Read")
        .into_inner();
    while stream.message().await.expect("a chunk").is_some() {}

    let blobs = record(&cell, id).await;
    let c = blobs.counts();
    assert_eq!((c.blobs_asked, c.blobs_missing), (2, 1));
    assert_eq!(c.bytes_uploaded, blob.data.len() as u64);
    assert_eq!(
        c.bytes_downloaded,
        (job.input.data.len() + blob.data.len()) as u64
    );
    assert!(blobs.rows().is_empty(), "blob calls make no rows");
}

/// Catches a timeout recorded as a plain failure, with a partial result (stdout the
/// CAS holds) or without one, and a refusal (no live worker can run the action)
/// recorded as anything but `refused` with the scheduler's reason, or with an attempt
/// it never had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeouts_and_refusals_are_recorded() {
    let cell = Cell::start_with_unservable_wait(std::time::Duration::from_millis(500)).await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let bare = Job::new("times out bare", &[]);
    let partial = Job::new("times out with stdout", &[]);
    let mac = Job::new("mac only", &[("OSFamily", "macos")]);
    for job in [&bare, &partial, &mac] {
        upload(&cell, BUILD, &job.blobs()).await;
    }

    let mut ops = execute(&cell, BUILD, &bare.action).await;
    let start = daemon.start().await;
    let report = failed(start.lease_id, Code::DeadlineExceeded);
    assert!(daemon.report(report).await.accepted);
    done(&mut ops).await;

    let mut ops = execute(&cell, BUILD, &partial.action).await;
    let start = daemon.start().await;
    let stdout = Blob::new("written before the timeout");
    cell.upload(&[&stdout]).await;
    let mut report = failed(start.lease_id, Code::DeadlineExceeded);
    report.action_result = Some(ActionResult {
        stdout_digest: Some(stdout.proto.clone()),
        ..Default::default()
    });
    assert!(daemon.report(report).await.accepted);
    let answered = response(&done(&mut ops).await);
    assert!(answered.result.is_some(), "a partial result: {answered:?}");

    let mut ops = execute(&cell, BUILD, &mac.action).await;
    let refused = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(refused.code, Code::FailedPrecondition as i32);

    let inv = record(&cell, BUILD).await;
    let outcomes: Vec<_> = inv.rows().iter().map(|r| r.outcome.clone()).collect();
    let [bare_row, partial_row, mac_row] = inv.rows() else {
        panic!("three rows: {outcomes:?}");
    };
    assert_eq!(bare_row.outcome, Some(Outcome::TimedOut));
    assert_eq!(partial_row.outcome, Some(Outcome::TimedOut));
    let end = partial_row.attempts.last().and_then(|a| a.end.clone());
    assert_eq!(
        end,
        Some(AttemptEnd::Failed(Failure::Timeout, String::new()))
    );
    let Some(Outcome::Refused(why)) = &mac_row.outcome else {
        panic!("refused: {:?}", mac_row.outcome);
    };
    assert!(refused.message.starts_with(why.as_str()), "{why}");
    assert!(mac_row.attempts.is_empty(), "no lease was granted");
    assert_eq!(inv.counts().failed, 3);
}

/// Catches farm events sent for operations no recorded call waits on (plant: no
/// `recorded` check in `State::record_*`, or `record_submit` noting an operation
/// for a call without an id): an id-less action that is granted, requeued, run and
/// finished, and one that is refused, send nothing, so the farm builds no listing
/// for them under its lock. A call with an id is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operations_without_an_id_send_no_events() {
    let cell = Cell::start_with_unservable_wait(std::time::Duration::from_millis(500)).await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let run = Job::new("id-less, requeued once", &[]);
    let mac = Job::new("id-less, mac only", &[("OSFamily", "macos")]);
    cell.upload(&run.blobs()).await;
    cell.upload(&mac.blobs()).await;

    let mut ops = cell.execute(&run.action).await;
    let first = daemon.start().await;
    let lost = worker::Result {
        lost: worker::Lost::Fenced as i32,
        ..failed(first.lease_id, Code::Unavailable)
    };
    assert!(daemon.report(lost).await.accepted);
    let second = daemon.start().await;
    let result = output(&cell, "id-less", 0).await;
    assert!(daemon.report(ran(second.lease_id, &result)).await.accepted);
    done(&mut ops).await;
    let mut ops = cell.execute(&mac.action).await;
    done(&mut ops).await;
    cell.recorder.caught_up().await;
    assert_eq!(cell.recorder.sent(), 0, "no event for an id-less operation");

    upload(&cell, BUILD, &[&Blob::new("with an id")]).await;
    assert_eq!(cell.recorder.sent(), 1);
}
