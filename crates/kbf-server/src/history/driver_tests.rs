//! The writer task's start, stop and drop. Each test names the defect it catches.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_history::{HistoryRoot, IndexRoot};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{Abort, FINAL_STEP, Options, Running, STEP_EVERY, finish, over, spawn};

/// Sets its flag when it is dropped: a task's future is dropped when the task ends or is
/// aborted.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Catches: a stop that waits for a last step that never ends (an object store that
/// never answers would hold the server's exit for ever), that gives up sooner than the
/// budget, or that leaves the task running after it gave up. Time is the paused clock's.
#[tokio::test(start_paused = true)]
async fn a_stop_gives_up_on_a_last_step_that_never_ends() {
    let (stop, stopped) = oneshot::channel::<()>();
    let gone = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&gone));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let running = Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    };
    let begun = Instant::now();
    finish(Some(running)).await;
    assert_eq!(begun.elapsed(), FINAL_STEP);
    assert!(stopped.await.is_ok(), "the task was told to stop");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(gone.load(Ordering::SeqCst), "the task was aborted");
}

/// Catches: a stop that reports a writer that finished in time as one it gave up on (the
/// warning is for the last step that ran over), or that does not wait for the writer to
/// finish before it returns: the task is done when the stop is.
#[tokio::test]
async fn a_stop_waits_for_a_last_step_that_ends() {
    let (stop, stopped) = oneshot::channel::<()>();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let handle = tokio::spawn(async move {
        flag.store(true, Ordering::SeqCst);
    });
    finish(Some(Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    }))
    .await;
    assert!(
        done.load(Ordering::SeqCst),
        "the last step ran before the stop returned"
    );
    assert!(stopped.await.is_ok(), "the task was told to stop");
}

/// Catches: a task that outlives its `Running` (a server that dropped its serving future
/// would leave a writer running against the stores), and a stop of nothing that waits.
#[tokio::test(start_paused = true)]
async fn dropping_a_running_writer_aborts_it_and_nothing_stops_at_once() {
    let (stop, _stopped) = oneshot::channel::<()>();
    let gone = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&gone));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    drop(Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(gone.load(Ordering::SeqCst), "aborted when dropped");
    assert!(spawn(None).is_none());
    let begun = Instant::now();
    finish(None).await;
    assert_eq!(begun.elapsed(), std::time::Duration::ZERO);
}

/// Catches: a writer id that is not the start's time (two starts would share one, and
/// their objects would collide on a key), and a step cadence that is not the one the index's transaction budget assumes.
#[test]
#[expect(
    clippy::clone_on_copy,
    reason = "the derived clone is the code the coverage ratchet counts, and is called here"
)]
fn a_start_is_named_by_its_time_and_steps_twice_a_second() {
    let a = Options::default();
    // Options are copied and shown as what they are.
    let copy = a.clone();
    assert_eq!(copy.writer, a.writer);
    assert!(format!("{copy:?}").contains("step_every"));
    assert_ne!(a.writer.0, 0, "microseconds since the epoch");
    assert_eq!(a.step_every, STEP_EVERY);
    assert_eq!(STEP_EVERY, std::time::Duration::from_millis(500));
}

/// Catches: a history that cannot be shown in a failure message (the tests that expect
/// it to be refused print it), and counters that are not those of the writer's own queue:
/// a new history has dropped nothing, holds nothing and says so in the Prometheus text.
#[test]
fn a_new_history_shows_as_itself_and_counts_nothing() {
    let store: Arc<dyn kbf_state::Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root");
    let history = over(
        MemoryStore::new(Capabilities::default()),
        root,
        store,
        IndexRoot::default(),
        Options::default(),
    );
    assert_eq!(format!("{history:?}"), "History { .. }");
    let stats = history.stats();
    assert_eq!(stats.dropped("queue"), 0);
    assert_eq!(stats.dropped("no such stage"), 0);
    let mut text = String::new();
    stats.render(&mut text);
    assert!(text.contains("kbf_history_queue_bytes 0\n"), "{text}");
    assert!(
        text.contains("# TYPE kbf_history_dropped_total counter\n"),
        "{text}"
    );
    let _tap = history.tap();
}
