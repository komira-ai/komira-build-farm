//! The failure-log route's read of a copy, under a budget, for one instance, by range. Each
//! test names the defect it catches. Time is the paused clock's.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_TYPE, RETRY_AFTER,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use kbf_history::records::{FlogStream, ObjRow};
use kbf_history::{
    FlogParts, FlogStore, HistoryIndex, HistoryRoot, IndexRoot, Inline, Reader, RunId, StreamId,
    WriterId,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use prost::Message as _;
use serde_json::Value;

use super::{PREFIX, ReadBudget, parse_path, serve};
use crate::history::{HistoryReads, RunIndex, Shared};
use crate::http_blobs::READ_BUDGET_WAIT;

/// A memory store whose reads fail once `fail_after` of them have been answered.
struct Flaky {
    memory: MemoryStore,
    reads: std::sync::atomic::AtomicU64,
    fail_after: std::sync::atomic::AtomicU64,
}

impl ObjectStore for Flaky {
    fn capabilities(&self) -> Capabilities {
        self.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: bytes::Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.memory.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<bytes::Bytes, ObjectStoreError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.reads.fetch_add(1, SeqCst) >= self.fail_after.load(SeqCst) {
            return Err(ObjectStoreError::Protocol("planted failure".into()));
        }
        self.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.memory.list(prefix, after, max_keys).await
    }
}

fn run() -> RunId {
    RunId::new(1_792_108_800_005, StreamId(0x80_0001), 3)
}

fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

/// Reads over a store holding the failure-log copy of `run()`'s stdout (the bytes given;
/// stderr is not held) for instance `main`, and the memory store under it.
async fn reads(stdout: &[u8]) -> (HistoryReads, Arc<Flaky>) {
    let objects = Arc::new(Flaky {
        memory: MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }),
        reads: std::sync::atomic::AtomicU64::new(0),
        fail_after: std::sync::atomic::AtomicU64::new(u64::MAX),
    });
    let shared = Shared(Arc::clone(&objects));
    let state: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let index = HistoryIndex::new(Arc::clone(&state), IndexRoot::default(), Arc::new(Inline));
    let flog = FlogStore::new(shared.clone(), root(), WriterId(1));
    let parts = FlogParts {
        instance: "main",
        stdout: Some(stdout),
        ..FlogParts::default()
    };
    let written = flog.put(flog.encode(run(), &parts)).await.expect("written");
    let row = ObjRow {
        footer_offset: written.footer_offset,
        footer_len: written.footer_len,
        bytes: written.bytes,
    };
    let key = IndexRoot::default().obj_flog(run());
    state
        .txn(Txn::new().put(key, row.encode_to_vec(), Cond::Any))
        .expect("a write")
        .expect("no conflict");
    let reader = Reader::new(shared, root(), index);
    let reads = HistoryReads::new(reader, Arc::new(RunIndex::default()));
    (reads, objects)
}

async fn body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body")
        .to_vec()
}

async fn json(response: Response) -> Value {
    serde_json::from_slice(&body(response).await).expect("JSON")
}

fn range(text: &'static str) -> Option<HeaderValue> {
    Some(HeaderValue::from_static(text))
}

async fn read(
    reads: &HistoryReads,
    budget: &ReadBudget,
    instance: &str,
    range: Option<&HeaderValue>,
) -> Result<Response, Response> {
    serve(reads, budget, (run(), FlogStream::Stdout), instance, range).await
}

fn log() -> Vec<u8> {
    (0..300_000).map(|i| (i % 251) as u8).collect()
}

/// Catches: the log's bytes changed on the way out (a piece boundary at 64 KiB shifted), a
/// range that is not the range (a `206` for the wrong bytes, a `Content-Range` that does
/// not say them), a whole log sent for a range or a range for a whole log, an
/// unsatisfiable range answered `200`, several ranges honoured, and a body that is not an
/// attachment of octet-stream bytes.
#[tokio::test]
async fn a_log_is_sent_whole_or_by_its_one_range_as_an_attachment() {
    let log = log();
    let (reads, _) = reads(&log).await;
    let budget = ReadBudget::default();
    let whole = read(&reads, &budget, "main", None).await.expect("served");
    assert_eq!(whole.status(), StatusCode::OK);
    let head = whole.headers();
    assert_eq!(head[CONTENT_TYPE], "application/octet-stream");
    assert_eq!(head[CONTENT_DISPOSITION], "attachment");
    assert_eq!(head[ACCEPT_RANGES], "bytes");
    assert!(head.get(CONTENT_RANGE).is_none());
    assert!(body(whole).await == log, "the whole log, unchanged");
    for (text, first, last) in [
        ("bytes=0-0", 0, 0),
        ("bytes=65535-65536", 65_535, 65_536),
        ("bytes=100000-", 100_000, 299_999),
        ("bytes=-10", 299_990, 299_999),
        ("bytes=299999-999999", 299_999, 299_999),
    ] {
        let part = read(&reads, &budget, "main", range(text).as_ref())
            .await
            .expect("served");
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT, "{text}");
        assert_eq!(
            part.headers()[CONTENT_RANGE],
            format!("bytes {first}-{last}/300000"),
            "{text}"
        );
        assert!(body(part).await == log[first..=last], "{text}");
    }
    let past = read(&reads, &budget, "main", range("bytes=300000-").as_ref())
        .await
        .expect("answered");
    assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.headers()[CONTENT_RANGE], "bytes */300000");
    assert!(body(past).await.is_empty());
    // Several ranges are ignored, as the blob route ignores them.
    let several = read(&reads, &budget, "main", range("bytes=0-1,5-6").as_ref())
        .await
        .expect("served");
    assert_eq!(several.status(), StatusCode::OK);
    assert_eq!(body(several).await.len(), 300_000);
    assert_eq!(budget.available_permits(), 1 << 30, "every permit is back");
}

/// Catches: an empty log read from the store (a range read of nothing), refused, or given
/// a budget share of zero bytes that could not be had; a range of it satisfiable.
#[tokio::test]
async fn an_empty_log_is_an_empty_answer() {
    let (reads, _) = reads(b"").await;
    // A budget with nothing in it: an empty log takes none.
    let budget = ReadBudget::new(0);
    let whole = read(&reads, &budget, "main", None).await.expect("served");
    assert_eq!(whole.status(), StatusCode::OK);
    assert!(body(whole).await.is_empty());
    let part = read(&reads, &budget, "main", range("bytes=0-5").as_ref())
        .await
        .expect("answered");
    assert_eq!(part.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(part.headers()[CONTENT_RANGE], "bytes */0");
}

/// Catches (security): a log served to a caller authorized on another instance than the
/// run's, with the run found; a log that is not held (stderr here, which the run never
/// wrote) answered as a log; the cause or the bytes in the refusal.
#[tokio::test]
async fn a_log_of_another_instance_or_not_held_is_no_log() {
    let (reads, _) = reads(b"secret stdout").await;
    let budget = ReadBudget::default();
    let other = read(&reads, &budget, "other", None)
        .await
        .expect_err("refused");
    assert_eq!(other.status(), StatusCode::NOT_FOUND);
    let text = json(other).await;
    assert_eq!(text["error"], "no_log");
    assert_eq!(text["log"], "stdout");
    assert!(!text.to_string().contains("secret"));
    let stderr = serve(&reads, &budget, (run(), FlogStream::Stderr), "main", None)
        .await
        .expect_err("not held");
    assert_eq!(json(stderr).await["log"], "stderr");
    let elsewhere = serve(
        &reads,
        &budget,
        (
            RunId::new(1_792_108_800_005, StreamId(9), 9),
            FlogStream::Stdout,
        ),
        "main",
        None,
    )
    .await
    .expect_err("no object");
    assert_eq!(elsewhere.status(), StatusCode::NOT_FOUND);
}

/// Catches: a read that takes more memory than the budget has (a looping caller of this
/// route would take every byte the blob route and the history writer need), one that waits
/// for it for ever, a wait that does not end in a `503` with `Retry-After` and `busy`, one
/// that could never fit told to try again, and a permit not given back after the answer.
#[tokio::test(start_paused = true)]
async fn a_read_the_budget_cannot_hold_is_busy_and_one_that_never_fits_is_503() {
    let log = log();
    let (reads, _) = reads(&log).await;
    // The whole budget is held by another read.
    let budget = ReadBudget::new(400_000);
    let held = budget
        .acquire(400_000, READ_BUDGET_WAIT)
        .await
        .expect("the budget is free");
    let begun = tokio::time::Instant::now();
    let busy = read(&reads, &budget, "main", None).await.expect_err("busy");
    assert_eq!(begun.elapsed(), READ_BUDGET_WAIT);
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.headers()[RETRY_AFTER], "1");
    let text = json(busy).await;
    assert_eq!(text["busy"], true, "{text}");
    drop(held);
    let served = read(&reads, &budget, "main", None).await.expect("served");
    assert_eq!(body(served).await.len(), log.len());
    assert_eq!(budget.available_permits(), 400_000);
    // A budget smaller than the read: it can never be admitted.
    let small = ReadBudget::new(1000);
    let never = read(&reads, &small, "main", None).await.expect_err("never");
    assert_eq!(never.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(never.headers()[RETRY_AFTER], "1");
    let text = json(never).await;
    assert_eq!(text["busy"], false, "{text}");
    assert!(
        text["error"]
            .as_str()
            .unwrap_or_default()
            .contains("more than")
    );
    // A range that fits is read in a budget too small for the whole.
    let part = read(&reads, &small, "main", range("bytes=10-19").as_ref())
        .await
        .expect("served");
    assert_eq!(body(part).await, log[10..20]);
}

/// Catches: a block that cannot be read after the footer was (the object is there, a part of
/// it is not) answered as a short log or an empty one, or with the store's message.
#[tokio::test]
async fn a_block_that_cannot_be_read_is_503_without_the_cause() {
    let (reads, objects) = reads(&log()).await;
    // The footer is read, and then every read of the blocks fails.
    objects
        .fail_after
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let answer = read(&reads, &ReadBudget::default(), "main", None)
        .await
        .expect_err("unreadable");
    assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
    let text = json(answer).await;
    assert_eq!(text["error"], "build history cannot be read right now");
}

/// Catches: a copy whose object is gone (expired) answered as an empty or absent log (a
/// `404` for what is held), or with the store's message in the answer.
#[tokio::test]
async fn a_copy_that_cannot_be_read_is_503_without_the_cause() {
    use kbf_objstore::ObjectStore as _;
    let (reads, objects) = reads(&log()).await;
    objects.delete(&root().flog(run())).await.expect("deleted");
    let answer = read(&reads, &ReadBudget::default(), "main", None)
        .await
        .expect_err("unreadable");
    assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
    let text = json(answer).await;
    assert_eq!(text["error"], "build history cannot be read right now");
}

/// Catches: a path that names something else read as a log (another stream, a third
/// segment, a run id of the wrong length), and `logs` spelt another way.
#[test]
fn the_path_names_a_run_and_one_of_its_three_logs() {
    let id = run().to_string();
    for (stream, name) in [
        (FlogStream::Stdout, "stdout"),
        (FlogStream::Stderr, "stderr"),
        (FlogStream::Result, "result"),
    ] {
        let path = format!("/v1/runs/{id}/logs/{name}");
        let parsed = parse_path(&path).expect("a log");
        assert_eq!(parsed, (run(), stream));
    }
    for bad in [
        format!("/v1/runs/{id}/logs/stdin"),
        format!("/v1/runs/{id}/logs/stdout/more"),
        format!("/v1/runs/{id}/log/stdout"),
        format!("/v1/runs/{id}/logs/"),
        "/v1/runs/abc/logs/stdout".to_owned(),
        format!("/v1/runs/{}/logs/stdout", id.to_uppercase()),
        "/v1/runs//logs/stdout".to_owned(),
    ] {
        let why = parse_path(&bad).expect_err(&bad);
        assert!(why.contains(&bad[PREFIX.len()..]), "{bad}: {why}");
    }
}
