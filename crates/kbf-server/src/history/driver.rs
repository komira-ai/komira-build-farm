//! The history writer's task: a step every [`STEP_EVERY`], and one last bounded step,
//! with every open invocation closed, when the server stops.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::Tap;
use kbf_history::{
    EventQueue, HistoryIndex, HistoryRoot, IndexRoot, Limits, Minter, Offload, Offloaded,
    QueueLimits, Writer, WriterId,
};
use kbf_objstore::ObjectStore;
use kbf_state::Store;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::stats::Stats;
use super::tap::HistoryTap;

/// How often the writer takes its queue: twice a second, which keeps the index
/// transactions at the two a second the writer's own bound allows.
pub const STEP_EVERY: Duration = Duration::from_millis(500);

/// How long the last step, at a stop, may take before the task is given up.
const FINAL_STEP: Duration = Duration::from_secs(5);

/// What a history writer is made with besides its stores.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The id the objects it writes carry: new for each start.
    pub writer: WriterId,
    /// The writer's bounds.
    pub limits: Limits,
    /// The queue's bounds.
    pub queue: QueueLimits,
    /// The time between steps.
    pub step_every: Duration,
}

impl Default for Options {
    fn default() -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            // Microseconds since the epoch: no two starts of one process, or of two
            // processes a microsecond apart, share one.
            writer: WriterId(u64::try_from(since.as_micros()).unwrap_or(u64::MAX)),
            limits: Limits::default(),
            queue: QueueLimits::default(),
            step_every: STEP_EVERY,
        }
    }
}

/// A history writer that has been made and not started: its tap for the recorder, its
/// counters for `/metrics`, and the task that writes.
pub struct History {
    tap: Arc<HistoryTap>,
    stats: Arc<Stats>,
    task: Pin<Box<dyn Future<Output = ()> + Send>>,
    stop: oneshot::Sender<()>,
}

impl History {
    /// The tap the recorder's task is given ([`kbf_front::Recording::with_tap`]).
    #[must_use]
    pub fn tap(&self) -> Arc<dyn Tap> {
        let tap: Arc<dyn Tap> = self.tap.clone();
        tap
    }

    /// What history counts.
    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History").finish_non_exhaustive()
    }
}

/// Index calls on the runtime's blocking pool: the state store's calls block.
struct Blocking;

impl Offload for Blocking {
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
        let handle = tokio::task::spawn_blocking(job);
        Box::pin(async move {
            // A job that never ran (the pool is shutting down) is answered by the index
            // as a failed call.
            let _ = handle.await;
        })
    }
}

/// A history writer over `objects` (under `root`) and the rows of `index_root` in
/// `state`, not yet started: [`spawn`] starts it. `objects` is wrapped in the writer's
/// own `Scoped` store, which refuses every key outside `<root>history/`.
pub fn over<O: ObjectStore>(
    objects: O,
    root: HistoryRoot,
    state: Arc<dyn Store>,
    index_root: IndexRoot,
    options: Options,
) -> History {
    let queue = Arc::new(EventQueue::new(options.queue));
    let minter = Arc::new(Minter::new(0));
    let index = HistoryIndex::new(state, index_root, Arc::new(Blocking));
    let writer = Writer::new(
        objects,
        root,
        index,
        options.writer,
        Arc::clone(&minter),
        Arc::clone(&queue),
        options.limits,
    );
    let tap = Arc::new(HistoryTap::new(Arc::clone(&queue), minter));
    let stats = Arc::new(Stats::new(queue));
    let (stop, stopped) = oneshot::channel();
    let task = Box::pin(run(
        writer,
        Arc::clone(&tap),
        Arc::clone(&stats),
        options.step_every,
        stopped,
    ));
    History {
        tap,
        stats,
        task,
        stop,
    }
}

/// The wall clock in milliseconds, as the writer is given it.
fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

async fn run<O: ObjectStore>(
    mut writer: Writer<O>,
    tap: Arc<HistoryTap>,
    stats: Arc<Stats>,
    every: Duration,
    mut stopped: oneshot::Receiver<()>,
) {
    // The first step is one interval after the start: a step with nothing queued yet
    // would only reserve a stream block the first record will ask for anyway.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let publish = |writer: &Writer<O>| {
        stats.publish(
            writer.drops(),
            writer.pending_rows(),
            writer.open_invocations(),
        );
    };
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => break,
            _ = tick.tick() => {}
        }
        writer.step(now_ms()).await;
        publish(&writer);
    }
    // Every invocation offered a record is closed, so the step flushes its chunk.
    tap.close_all();
    writer.step(now_ms()).await;
    publish(&writer);
}

/// A spawned task, aborted when this is dropped.
struct Abort(JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A started history writer: dropped, its task is aborted.
pub(crate) struct Running {
    handle: Abort,
    stop: oneshot::Sender<()>,
}

/// Starts `history`'s task on the runtime, if there is a history.
pub(crate) fn spawn(history: Option<History>) -> Option<Running> {
    let History { task, stop, .. } = history?;
    Some(Running {
        handle: Abort(tokio::spawn(task)),
        stop,
    })
}

/// Stops `running` as a server's stop does: its writer takes one last step with every
/// open invocation closed, for at most [`FINAL_STEP`].
pub(crate) async fn finish(running: Option<Running>) {
    let Some(Running { mut handle, stop }) = running else {
        return;
    };
    let _ = stop.send(());
    if tokio::time::timeout(FINAL_STEP, &mut handle.0)
        .await
        .is_err()
    {
        // Dropping `handle` aborts the task.
        tracing::warn!("history: the last step took over {FINAL_STEP:?}; it was dropped");
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
