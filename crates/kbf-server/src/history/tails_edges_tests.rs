//! The pool's edges: a pool with no workers, a blob over the whole budget, an encoder that
//! fails and an ActionResult the budget cannot serve. Each test names the defect it
//! catches. The fakes are those of the tests above.

use super::*;

/// Catches: a job offered to a pool with no workers (they were dropped, so the channel is
/// closed) lost with the run (the tap must offer it itself) or counted as a success; and a
/// worker that outlives its pool (the tap's end dropped), which would hold the receiver
/// and the runtime for ever.
#[tokio::test]
async fn a_pool_with_no_workers_gives_the_job_back_and_workers_end_with_the_pool() {
    let rig = rig(options(), 1 << 30);
    let parts = || Parts {
        queue: Arc::clone(&rig.queue),
        blobs: rig.blobs.clone(),
        budget: rig.budget.clone(),
        flog: Arc::new(FlogStore::new(
            Shared(Arc::clone(&rig.objects)),
            root(),
            WriterId(1),
        )),
        stats: Arc::clone(&rig.stats),
        open: Arc::clone(&rig.open),
    };
    // Workers that were never started and are dropped: the channel is closed.
    let (tails, workers) = pool(parts(), options());
    drop(workers);
    let back = tails
        .submit(job("a", 1, [None; 4]))
        .expect_err("no worker is left to take it");
    assert_eq!(back.record.run_id(), id(1));
    assert_eq!(rig.stats.dropped("tail"), 1);
    tails.quiesce().await;
    // Workers whose pool is dropped end.
    let (tails, workers) = pool(parts(), options());
    let handles = workers.spawn();
    drop(tails);
    for handle in handles {
        let ended = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(ended.expect("the worker ends").is_ok());
    }
}

/// Catches: a blob larger than the whole budget waiting for a share it can never have
/// (it is `unreadable` at once), and an ActionResult that cannot be read taking the run's
/// tails with it, or being copied as an empty one.
#[tokio::test]
async fn a_blob_over_the_whole_budget_and_an_unreadable_result_are_left_out() {
    let rig = rig(options(), 5);
    let big = rig.blobs.put(&[1; 10]);
    let missing = WireDigest::new([9; 32], 3);
    submit(&rig, job("a", 1, [Some(&big), None, Some(&missing), None])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    let (_, failed) = run_of(&events);
    let tail = failed.stdout.as_ref().expect("a tail record");
    assert_eq!(tail.omitted(), TailOmitted::Unreadable);
    assert_eq!(
        rig.blobs.reads.load(Ordering::SeqCst),
        1,
        "only the result was asked for"
    );
    assert_eq!(events.len(), 1, "nothing was copied: {events:?}");
}

/// A sink whose encoding panics, as a bug in the encoder would.
struct Panics;

impl FlogSink for Panics {
    fn encode(&self, _: RunId, _: &kbf_history::FlogParts<'_>) -> kbf_history::FlogObject {
        panic!("planted: the encoder fails");
    }

    fn put(
        &self,
        _: kbf_history::FlogObject,
    ) -> BoxFuture<'_, Result<kbf_history::FlogRef, ObjectStoreError>> {
        Box::pin(async { Err(ObjectStoreError::Protocol("unused".into())) })
    }
}

/// Catches: an encoder that fails (a panic on the blocking pool) taking the worker, and
/// with it the run, down: the copy is lost and counted (`stage="put"`), and the run
/// arrives with its tails.
#[tokio::test]
async fn a_copy_that_cannot_be_encoded_is_counted_and_the_run_still_arrives() {
    let queue = Arc::new(EventQueue::new(QueueLimits::default()));
    let stats = Arc::new(Stats::new(Arc::clone(&queue)));
    let blobs = Arc::new(Blobs::default());
    let out = blobs.put(b"out");
    let open = Arc::new(OpenSet::default());
    let parts = Parts {
        queue: Arc::clone(&queue),
        blobs: blobs.clone(),
        budget: ReadBudget::default(),
        flog: Arc::new(Panics),
        stats: Arc::clone(&stats),
        open: Arc::clone(&open),
    };
    let (tails, workers) = pool(parts, options());
    let handles = workers.spawn();
    open.add("a");
    tails
        .submit(job("a", 1, [Some(&out), None, None, None]))
        .expect("queued");
    tails.quiesce().await;
    assert_eq!(stats.dropped("put"), 1);
    let events = queue.drain();
    assert_eq!(events.len(), 1, "{events:?}");
    handles.iter().for_each(tokio::task::JoinHandle::abort);
}

/// Catches: an ActionResult the read budget cannot serve in time failing the run, waiting
/// for ever, or copied as an empty one: the run arrives with its tails (none here) and no
/// copy, like a log that was `busy`.
#[tokio::test(start_paused = true)]
async fn a_result_the_budget_cannot_serve_in_time_is_left_out() {
    let rig = rig(options(), 100);
    let held = rig.blobs.put(&[1; 100]);
    rig.blobs.stall(&held);
    let result = rig.blobs.put(&[2; 10]);
    submit(&rig, job("a", 1, [Some(&held), None, None, None])).expect("queued");
    submit(&rig, job("b", 2, [None, None, Some(&result), None])).expect("queued");
    tokio::time::sleep(options().wait * 4).await;
    let events = events(&rig);
    assert_eq!(events.len(), 1, "{events:?}");
    let (record, failed) = run_of(&events);
    assert_eq!(record.run_id(), id(2));
    assert_eq!(failed.flog_omitted(), FlogOmitted::No);
    assert_eq!(rig.objects.puts.load(Ordering::SeqCst), 0);
}

/// Catches: a run whose only copyable log is its ActionResult (it printed nothing) given no
/// copy, because the logs were taken for the only reason to write an object.
#[tokio::test]
async fn a_run_that_printed_nothing_still_keeps_its_result() {
    let rig = rig(options(), 1 << 30);
    let result = rig.blobs.put(b"an ActionResult");
    submit(&rig, job("a", 1, [None, None, Some(&result), None])).expect("queued");
    rig.tails.quiesce().await;
    let events = events(&rig);
    assert_eq!(events.len(), 2, "{events:?}");
    let s = summary_of(&rig, &events[0]);
    assert_eq!(
        (s.stdout_len, s.stderr_len, s.result_len),
        (None, None, Some(15))
    );
}
