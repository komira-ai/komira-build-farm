//! A `crun run` the driver started: [`Running`].

use std::time::{Duration, Instant};

use crate::oci::RunEnd;

/// A `crun run` driven by the `runc` crate's async client in a thread of its own, on a
/// one-thread runtime that thread owns. It ends when the container does, and kills
/// nothing itself: the lease cgroup is killed to end the container.
///
/// A thread rather than a task of the daemon's runtime: the lease's clean runs inside
/// `Drop`, blocking, and must wait for `crun` to be gone before it removes anything
/// (kbf#157). A task is polled by the runtime the clean is blocking, which on a
/// one-thread runtime, or when every worker is cleaning, would never finish; a thread
/// that owns the child can always be waited for. The cost is one OS thread per running
/// action, for the length of the action.
#[derive(Debug)]
pub(crate) struct Running {
    thread: std::thread::JoinHandle<()>,
    /// `None` until the run has ended, then how it did. Dropped unanswered if the thread
    /// panics.
    ended: tokio::sync::watch::Receiver<Option<RunEnd>>,
    abort: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Running {
    /// Runs `work` (a `crun run`) on a new thread named `name`. Dropping `work`, which
    /// aborting does, kills `crun` (`oci::crun::KillOnDrop`).
    pub(crate) fn spawn<F>(name: String, work: F) -> Result<Self, String>
    where
        F: std::future::Future<Output = RunEnd> + Send + 'static,
    {
        let thread = std::thread::Builder::new().name(name);
        Self::start(thread, one_thread_runtime, Box::pin(work))
    }

    /// [`Running::spawn`] on `thread`, with `runtime` making the thread's runtime: the two
    /// places it can fail, each a test's to cause. Not generic, so one build of it is
    /// what every test (and the coverage count) sees.
    fn start(
        thread: std::thread::Builder,
        runtime: fn() -> std::io::Result<tokio::runtime::Runtime>,
        work: std::pin::Pin<Box<dyn std::future::Future<Output = RunEnd> + Send>>,
    ) -> Result<Self, String> {
        let (end_tx, ended) = tokio::sync::watch::channel(None);
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
        let body = move || {
            let end = match runtime() {
                Ok(runtime) => runtime.block_on(async {
                    tokio::select! {
                        end = work => end,
                        _ = abort_rx => RunEnd::Failed("crun run was aborted".to_owned()),
                    }
                }),
                Err(e) => RunEnd::Failed(format!("the runtime of the crun thread: {e}")),
            };
            end_tx.send_replace(Some(end));
        };
        let thread = thread
            .spawn(body)
            .map_err(|e| format!("start the crun thread: {e}"))?;
        Ok(Self {
            thread,
            ended,
            abort: Some(abort_tx),
        })
    }

    /// Waits until the run has ended. Cancel-safe; may be called again.
    pub(crate) async fn wait(&mut self) {
        // Err: the thread panicked and dropped its answer; `run_end` says so.
        let _ = self.ended.wait_for(Option::is_some).await;
    }

    /// Ends the run by force: the abort kills `crun`.
    pub(crate) async fn force(&mut self) {
        self.abort();
        self.wait().await;
    }

    /// How the run ended, once [`Running::wait`] returned. A run that did not say (its
    /// thread panicked) is a failure.
    pub(crate) fn run_end(&self) -> RunEnd {
        self.ended
            .borrow()
            .clone()
            .unwrap_or_else(|| RunEnd::Failed("crun run did not say how it ended".to_owned()))
    }

    /// Aborts the run, which kills `crun`; nothing if it was aborted already.
    fn abort(&mut self) {
        let _ = self.abort.take().map(|abort| abort.send(()));
    }

    /// Waits at most `limit` for the run to be gone, polling every `pause`. The caller
    /// has killed the lease cgroup, which ends the container and so `crun`; a run still
    /// there at `limit` is aborted, which kills it. Blocking, without the async runtime:
    /// a dropped run's clean runs inside `Drop`.
    pub(crate) fn reap(&mut self, limit: Duration, pause: Duration) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        let mut aborted = false;
        while !self.thread.is_finished() {
            if Instant::now() >= deadline {
                self.abort();
                if aborted {
                    return Err(format!(
                        "crun run still runs {limit:?} after its cgroup was killed"
                    ));
                }
                aborted = true;
            }
            std::thread::sleep(pause);
        }
        Ok(())
    }
}

/// The runtime of a `crun` thread: one thread, with the drivers `crun`'s child needs.
fn one_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_fails() -> std::io::Result<tokio::runtime::Runtime> {
        Err(std::io::Error::other("no epoll"))
    }

    fn failed(says: &str) -> RunEnd {
        RunEnd::Failed(says.to_owned())
    }

    const SOON: Duration = Duration::from_secs(5);
    const PAUSE: Duration = Duration::from_millis(1);

    /// Catches a run whose answer is lost or wrong: the thread's own failures (no
    /// runtime, no thread), a panic in the work (no answer), an abort (twice is the same
    /// as once), and a normal end, each ending the run with what it was instead of a
    /// hang; and `wait`/`reap` that cannot be called again.
    #[tokio::test]
    async fn a_run_always_ends_with_an_answer() {
        let build = |name: &str| std::thread::Builder::new().name(name.to_owned());
        let idle = std::future::pending::<RunEnd>;

        let mut run =
            Running::start(build("no-runtime"), runtime_fails, Box::pin(idle())).expect("started");
        run.wait().await;
        run.wait().await;
        assert_eq!(
            run.run_end(),
            failed("the runtime of the crun thread: no epoll")
        );
        run.reap(SOON, PAUSE).expect("gone");
        run.reap(SOON, PAUSE).expect("gone again");

        let too_big = std::thread::Builder::new().stack_size(usize::MAX / 2);
        let refused = Running::start(too_big, one_thread_runtime, Box::pin(idle())).err();
        assert_eq!(
            refused.map(|e| e.starts_with("start the crun thread: ")),
            Some(true)
        );

        let panics = async {
            panic!("crun task panicked");
            #[allow(unreachable_code)]
            RunEnd::Exited(0)
        };
        let mut run =
            Running::start(build("panics"), one_thread_runtime, Box::pin(panics)).expect("started");
        run.wait().await;
        assert_eq!(run.run_end(), failed("crun run did not say how it ended"));
        run.reap(SOON, PAUSE).expect("gone");

        let mut run = Running::start(build("aborted"), one_thread_runtime, Box::pin(idle()))
            .expect("started");
        run.force().await;
        run.force().await;
        assert_eq!(run.run_end(), failed("crun run was aborted"));
        run.reap(SOON, PAUSE).expect("gone");

        let done = async { RunEnd::Exited(3) };
        let mut run =
            Running::start(build("done"), one_thread_runtime, Box::pin(done)).expect("started");
        run.wait().await;
        assert_eq!(run.run_end(), RunEnd::Exited(3));
        run.reap(SOON, PAUSE).expect("gone");
    }

    /// Catches a clean that gives up on a `crun` it cannot stop without saying so: a
    /// run stuck in a blocking call outlasts the limit, is aborted at it, and still
    /// outlasts the second wait, which is an error naming the limit.
    #[test]
    fn a_run_that_will_not_end_is_an_error_after_the_limit() {
        let stuck = async {
            // Blocks the thread: an abort cannot get in.
            std::thread::sleep(Duration::from_millis(600));
            RunEnd::Exited(0)
        };
        let mut run = Running::start(
            std::thread::Builder::new(),
            one_thread_runtime,
            Box::pin(stuck),
        )
        .expect("started");
        let why = run
            .reap(Duration::from_millis(100), Duration::from_millis(5))
            .expect_err("still running");
        assert!(why.contains("still runs"), "{why}");
        // It is waited for once it ends.
        run.reap(SOON, Duration::from_millis(5)).expect("ended");
    }
}
