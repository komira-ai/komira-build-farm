//! The progress tick on an open stream, on a paused clock: no socket is involved, so
//! the clock moves only when every task is waiting on a timer.

use std::time::Duration;

use futures::StreamExt as _;
use kbf_proto::reapi::ExecuteOperationMetadata;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_segments::sha256;
use prost::Message as _;
use tokio::sync::watch;

use super::{Progress, Stage, Ticket, closing, operations, progress};
use crate::metrics::CacheMetrics;

const EVERY: Duration = Duration::from_secs(30);

/// A ticket whose stage the test holds; the sender must outlive the stream.
fn ticket(stage: Stage) -> (watch::Sender<Stage>, Ticket) {
    ticket_named(stage, "operations/0")
}

/// [`ticket`] for the operation called `name`.
fn ticket_named(stage: Stage, name: &str) -> (watch::Sender<Stage>, Ticket) {
    let (tx, rx) = watch::channel(stage);
    let ticket = Ticket {
        name: name.to_owned(),
        instance: "main".to_owned(),
        action: sha256(b"action"),
        stage: rx,
    };
    (tx, ticket)
}

/// Catches (komira-build-farm#791): an open stream that sends nothing while its action
/// runs, so a load balancer's idle timeout cuts it and Buck2 (which never reconnects)
/// fails the action. An action that runs for ten intervals gets ten progress
/// operations after the first one, each the same operation, not done, at its current
/// stage. Mutant: no ticker (`Progress::off`, or a stream that does not select the
/// tick): the read times out on the paused clock and the test fails.
#[tokio::test(start_paused = true)]
async fn an_idle_stream_gets_a_progress_operation_every_tick() {
    let (_stage, ticket) = ticket(Stage::Executing);
    let (_closer, closing) = closing();
    let (ticker, progress) = progress(EVERY);
    let ticking = tokio::spawn(ticker.run());
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, progress, metrics.execute.wait());

    let first = ops.next().await.expect("an operation").expect("healthy");
    assert!(!first.done);
    for _ in 1..=10 {
        let update = tokio::time::timeout(10 * EVERY, ops.next())
            .await
            .expect("a progress operation (is the ticker running?)")
            .expect("the stream is open")
            .expect("healthy");
        assert_eq!(update.name, "operations/0");
        assert!(!update.done, "{update:?}");
        let meta = update.metadata.as_ref().expect("metadata");
        let meta = ExecuteOperationMetadata::decode(meta.value.as_slice()).expect("decodes");
        assert_eq!(meta.stage, ExecStage::Executing as i32);
    }
    ticking.abort();
}

/// The next item of `ops`; a panic if none comes within a hang deadline (not a speed
/// bound: on a regression the stream never ends and the paused clock would otherwise
/// wait for ever), saying `what` was awaited.
async fn read(
    ops: &mut super::OperationStream,
    what: &str,
) -> Option<Result<kbf_proto::google::longrunning::Operation, tonic::Status>> {
    tokio::time::timeout(10 * EVERY, ops.next())
        .await
        .unwrap_or_else(|_| panic!("no {what} within the hang deadline"))
}

/// Catches a stream that ignores its own stage change while ticks are also arriving:
/// the done operation is sent as the stream's last, and ends it.
#[tokio::test(start_paused = true)]
async fn a_progress_stream_still_ends_with_the_done_operation() {
    let (stage, ticket) = ticket(Stage::Queued);
    let (_closer, closing) = closing();
    let (ticker, progress) = progress(EVERY);
    let ticking = tokio::spawn(ticker.run());
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, progress, metrics.execute.wait());
    read(&mut ops, "first operation")
        .await
        .expect("open")
        .expect("healthy");
    read(&mut ops, "progress operation (is the ticker running?)")
        .await
        .expect("open")
        .expect("healthy");
    stage.send_replace(Stage::Done(super::Finished::Ran(Box::default())));
    let done = read(&mut ops, "done operation, with ticks arriving")
        .await
        .expect("open")
        .expect("healthy");
    assert!(done.done, "{done:?}");
    let end = read(&mut ops, "end of the stream").await;
    assert!(end.is_none(), "{end:?}");
    ticking.abort();
}

/// Without a ticker a stream sends only its stage changes.
#[tokio::test(start_paused = true)]
async fn a_stream_without_progress_sends_only_stage_changes() {
    let (stage, ticket) = ticket(Stage::Queued);
    let (_closer, closing) = closing();
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, Progress::off(), metrics.execute.wait());
    ops.next().await.expect("queued").expect("healthy");
    let quiet = tokio::time::timeout(100 * EVERY, ops.next()).await;
    assert!(quiet.is_err(), "an update with no change and no ticker");
    stage.send_replace(Stage::Executing);
    ops.next().await.expect("executing").expect("healthy");
}

/// Catches a drain that undoes a close: `drain` after `close` (a second signal, or a
/// deadline that ran out before the drain was noticed) must leave the streams closed,
/// not reopen them to wait for their action again. Draining twice changes nothing.
#[tokio::test(start_paused = true)]
async fn a_drain_after_a_close_leaves_it_closed() {
    let (closer, mut closing) = closing();
    assert!(!closing.draining());
    closer.drain();
    closer.drain();
    assert!(closing.draining());
    closer.close();
    closer.drain();
    assert!(closing.draining());
    tokio::time::timeout(Duration::from_secs(1), closing.closed())
        .await
        .expect("still closed after a drain");
}

/// Catches a zero interval that ticks without pause (a busy loop of progress
/// operations) or panics: zero is no progress, and the stream sends only stage changes.
#[tokio::test(start_paused = true)]
async fn a_zero_interval_never_ticks() {
    let (stage, ticket) = ticket(Stage::Queued);
    let (_closer, closing) = closing();
    let (ticker, progress) = progress(Duration::ZERO);
    let ticking = tokio::spawn(ticker.run());
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, progress, metrics.execute.wait());
    ops.next().await.expect("queued").expect("healthy");
    let quiet = tokio::time::timeout(100 * EVERY, ops.next()).await;
    assert!(quiet.is_err(), "an update with a zero interval");
    stage.send_replace(Stage::Executing);
    ops.next().await.expect("executing").expect("healthy");
    ticking.abort();
}

/// Catches `kbf_front_open_executions` that is not raised when the stream is made
/// (a stream the client has not read yet would be invisible to a drain that waits for
/// 0) or that stays raised however the stream ends. Each way a stream ends lowers it
/// exactly once: the done operation, a client that drops the stream, and
/// `Closer::close`. Mutants: `operations` not calling `OpenExecute::streaming` (the
/// first assertion fails); `Drop` not lowering it (the three settled assertions fail);
/// `streaming` raising twice per call (the first assertion reads 2).
#[tokio::test(start_paused = true)]
async fn open_executions_is_raised_with_the_stream_and_lowered_however_it_ends() {
    let metrics = CacheMetrics::default();
    let gauge = &metrics.execute.open_executions;

    // Done.
    let (stage, queued) = ticket(Stage::Queued);
    let (_closer, open) = closing();
    let mut ops = operations(queued, open, Progress::off(), metrics.execute.wait());
    assert_eq!(gauge.get(), 1, "raised before the first item is read");
    ops.next().await.expect("queued").expect("healthy");
    assert_eq!(gauge.get(), 1);
    stage.send_replace(Stage::Done(super::Finished::Ran(Box::default())));
    let done = ops.next().await.expect("done").expect("healthy");
    assert!(done.done);
    assert_eq!(gauge.get(), 0, "lowered when the done operation is made");
    assert!(ops.next().await.is_none());
    assert_eq!(gauge.get(), 0);

    // The client drops a stream it has read from, and one it never read.
    let (_stage, first) = ticket(Stage::Executing);
    let (_closer, open) = closing();
    let mut read = operations(first, open.clone(), Progress::off(), metrics.execute.wait());
    read.next().await.expect("executing").expect("healthy");
    let (_stage2, second) = ticket_named(Stage::Executing, "operations/1");
    let unread = operations(second, open, Progress::off(), metrics.execute.wait());
    assert_eq!(gauge.get(), 2);
    drop(read);
    assert_eq!(gauge.get(), 1);
    drop(unread);
    assert_eq!(gauge.get(), 0, "a stream dropped before its first item");

    // Close: the stream ends UNAVAILABLE and is gone.
    let (_stage, held) = ticket(Stage::Executing);
    let (closer, open) = closing();
    let mut ops = operations(held, open, Progress::off(), metrics.execute.wait());
    ops.next().await.expect("executing").expect("healthy");
    assert_eq!(gauge.get(), 1);
    closer.close();
    let gone = ops.next().await.expect("an end").expect_err("closed");
    assert_eq!(gone.code(), tonic::Code::Unavailable);
    assert_eq!(gauge.get(), 0, "lowered when the close ends the stream");
    assert!(ops.next().await.is_none());
    assert_eq!(gauge.get(), 0);
}
