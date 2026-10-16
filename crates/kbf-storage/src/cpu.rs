//! The bound on record hashing: CPU work for record digests runs on the blocking pool,
//! never on a runtime worker, and at most `--cpu-threads` of it at once, so a burst of
//! ranges, or a flush of a large block, cannot take every core from the rest of the
//! server. One pool per cache: the checks of an upload without a write tier and the
//! write tier's flusher share it.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Semaphore;
use tonic::Status;

use crate::cache::CacheError;

/// A hashing job that did not run to its end (it panicked).
#[derive(Clone, Debug, thiserror::Error)]
#[error("a hashing job failed: {0}")]
pub(crate) struct JobFailed(String);

impl From<JobFailed> for Status {
    fn from(failed: JobFailed) -> Self {
        Status::internal(failed.to_string())
    }
}

impl From<JobFailed> for kbf_objstore::ObjectStoreError {
    /// A streamed write's body whose hashing failed: the write ends, storing nothing.
    fn from(failed: JobFailed) -> Self {
        Self::Body(failed.to_string())
    }
}

impl From<JobFailed> for CacheError {
    fn from(failed: JobFailed) -> Self {
        CacheError::Internal(failed.to_string())
    }
}

/// A job for the pool. Boxed so the pool has one copy of its code, not one per closure.
pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// At most `threads` hashing jobs running at once.
#[derive(Debug)]
pub(crate) struct HashPool {
    permits: Arc<Semaphore>,
}

impl HashPool {
    pub(crate) fn new(threads: NonZeroUsize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(threads.get())),
        }
    }

    /// Runs `job` on the blocking pool once a permit is free; the permit is held for
    /// the job's whole run. The waiting and the job live in a task of their own, so a
    /// caller cancelled meanwhile (even before it had a permit) still sees the job run.
    ///
    /// # Errors
    /// [`JobFailed`] if the job panicked.
    pub(crate) async fn run(&self, job: Job) -> Result<(), JobFailed> {
        let permits = Arc::clone(&self.permits);
        tokio::spawn(async move {
            // The semaphore is never closed, so a permit always comes.
            let permit = permits.acquire_owned().await.ok();
            tokio::task::spawn_blocking(move || {
                job();
                drop(permit);
            })
            .await
        })
        .await
        .and_then(std::convert::identity)
        .map_err(|e| JobFailed(e.to_string()))
    }
}

impl HashPool {
    /// Runs `compute` as [`HashPool::run`] does and returns what it returned.
    ///
    /// # Errors
    /// [`JobFailed`] if the job panicked.
    pub(crate) async fn compute<T: Default + Send + 'static>(
        &self,
        compute: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, JobFailed> {
        let slot = Arc::new(Mutex::new(None));
        let there = Arc::clone(&slot);
        self.run(Box::new(move || {
            *there.lock().unwrap_or_else(PoisonError::into_inner) = Some(compute());
        }))
        .await?;
        // A job that ran to its end (else `run` failed) stored its result.
        let out = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        Ok(out.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    use tonic::Code;

    use super::*;

    fn two() -> NonZeroUsize {
        NonZeroUsize::MIN.saturating_add(1)
    }

    /// Catches a hashing job that panicked in a streamed write's body taken for
    /// anything but the body's failure (the write must end, storing nothing).
    #[test]
    fn a_failed_job_fails_a_streamed_body() {
        let failed = kbf_objstore::ObjectStoreError::from(JobFailed("it panicked".into()));
        assert_eq!(
            failed.to_string(),
            "the body of a streamed write failed: a hashing job failed: it panicked"
        );
    }

    /// Eight jobs offered at once to a pool of two run two at a time: never more (the
    /// bound), and two together (it is a pool, not a lock).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn at_most_the_configured_jobs_run_together() {
        let pool = Arc::new(HashPool::new(two()));
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (pool, now, peak) = (Arc::clone(&pool), Arc::clone(&now), Arc::clone(&peak));
            tasks.push(tokio::spawn(async move {
                pool.run(Box::new(move || {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    now.fetch_sub(1, Ordering::SeqCst);
                }))
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    /// `compute` returns what its closure returned, and a closure that panics is an
    /// INTERNAL status.
    #[tokio::test]
    async fn compute_returns_what_the_closure_returned() {
        let pool = HashPool::new(NonZeroUsize::MIN);
        assert_eq!(pool.compute(|| 7u32).await.unwrap(), 7);
        let failed = pool
            .compute(|| -> u32 { panic!("boom") })
            .await
            .unwrap_err();
        assert!(failed.to_string().starts_with("a hashing job failed"));
    }

    /// A job runs on a thread of its own, not on the thread that awaits it (here the
    /// runtime's only thread, standing for a worker).
    #[tokio::test]
    async fn jobs_run_off_the_awaiting_thread() {
        let here = std::thread::current().id();
        let there = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&there);
        HashPool::new(NonZeroUsize::MIN)
            .run(Box::new(move || {
                *slot.lock().unwrap() = Some(std::thread::current().id());
            }))
            .await
            .unwrap();
        let there = there.lock().unwrap().expect("the job ran");
        assert_ne!(here, there);
    }

    /// A job that panics is an INTERNAL status, not a hang or a panic of the caller, and
    /// its permit is returned: the next job still runs.
    #[tokio::test]
    async fn a_panicking_job_is_an_internal_error_and_frees_its_permit() {
        let pool = HashPool::new(NonZeroUsize::MIN);
        let failed = pool.run(Box::new(|| panic!("boom"))).await.unwrap_err();
        assert!(
            failed.to_string().starts_with("a hashing job failed"),
            "{failed:?}"
        );
        // Each caller's error type names it: INTERNAL to a daemon, internal to the cache.
        let status = Status::from(failed.clone());
        assert_eq!(status.code(), Code::Internal, "{status:?}");
        let cache = CacheError::from(failed);
        assert!(matches!(cache, CacheError::Internal(_)), "{cache:?}");
        let ran = Arc::new(AtomicUsize::new(0));
        let again = Arc::clone(&ran);
        tokio::time::timeout(
            Duration::from_secs(30),
            pool.run(Box::new(move || {
                again.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .await
        .expect("the permit came back")
        .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// A caller that gives up while its job still waits for a permit does not cancel
    /// the job: the segment's records stay claimed until the job records them.
    #[tokio::test]
    async fn a_cancelled_caller_does_not_cancel_its_job() {
        let pool = Arc::new(HashPool::new(NonZeroUsize::MIN));
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let first = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                pool.run(Box::new(move || {
                    let _ = hold_rx.recv();
                }))
                .await
            })
        };
        tokio::task::yield_now().await;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        // Gives up while the first job holds the only permit.
        let gave_up = tokio::time::timeout(
            Duration::from_millis(50),
            pool.run(Box::new(move || {
                let _ = done_tx.send(());
            })),
        )
        .await;
        assert!(gave_up.is_err());
        hold_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(30), done_rx)
            .await
            .expect("the abandoned job ran")
            .unwrap();
    }
}
