//! What a fill leaves in the cache's `tmp/` when it does not end as an entry: nothing,
//! however it ends. A file there is in no index, no budget and no fill metric, and is
//! cleared only at the next start, so every way out of a fill is driven here: each
//! error a CAS read ends with after writing part of the blob, a lease killed (its task
//! dropped, as the container runtime drops it on a kill during prepare) while its fill
//! is fetching, and one killed while the cache is about to hand it a checked file it
//! could not keep.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_daemon::cache::{Cache, Point};
use kbf_daemon::cas::{Cas, CasError, label};
use kbf_daemon::tree::{TreeError, materialize_cached};
use kbf_proto::reapi::Digest;
use tokio::io::AsyncWriteExt as _;

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, file, key, lease_dir, stop, tmp_files,
};
use crate::support::scratch;

const GIB: u64 = 1 << 30;

/// What [`Halfway`]'s next read of a file blob does once it wrote half the blob.
enum Then {
    /// Never finishes.
    Stall,
    /// Fails with this error.
    Fail(CasError),
}

/// A CAS whose next read of a file blob into a file writes half the blob and then does
/// what [`Then`] says; every read after it writes the whole blob.
#[derive(Default)]
struct Halfway {
    cas: CountingCas,
    then: Mutex<Option<Then>>,
}

impl Halfway {
    fn next(&self, then: Then) {
        *self.then.lock().expect("lock") = Some(then);
    }
}

impl Cas for Halfway {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.cas.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.cas.put(bytes).await
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        let bytes = self
            .cas
            .blobs
            .blob(digest)
            .ok_or(CasError::Missing(label(digest)))?;
        let then = self.then.lock().expect("lock").take();
        let Some(then) = then else {
            file.write_all(&bytes).await.expect("write");
            return file
                .flush()
                .await
                .map_err(|e| CasError::Write(label(digest), e.to_string()));
        };
        file.write_all(&bytes[..bytes.len() / 2])
            .await
            .expect("write");
        file.flush().await.expect("flush");
        match then {
            Then::Stall => std::future::pending().await,
            Then::Fail(error) => Err(error),
        }
    }
}

/// Waits, at most 10 seconds, until `tmp/` of the cache in `dir` is empty; whether it
/// emptied. A step on the blocking pool may still be running when its lease is gone.
async fn tmp_empties(dir: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while tmp_files(dir) > 0 {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// Catches a fill that leaves its file in `tmp/` when the CAS read fails (the
/// "only a write error removes the file" mutant, and the guard that removes nothing):
/// for each error a read can end with, after half the blob is written, `tmp/` is
/// empty and nothing is indexed; the lease fails with the CAS's error, except on a
/// write error (the cache's disk), where it fetches the blob into its own file.
#[tokio::test]
async fn a_fill_whose_read_fails_leaves_nothing_in_tmp() {
    let dir = scratch("cache-tmp-failed");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Halfway::default();
    let errors = [
        CasError::Missing("m".to_owned()),
        CasError::Corrupt("c".to_owned(), "h".to_owned()),
        CasError::Unavailable("u".to_owned(), "down".to_owned()),
        CasError::Read("r".to_owned(), "gone".to_owned()),
        CasError::Write("w".to_owned(), "no space".to_owned()),
    ];
    for (tag, error) in errors.into_iter().enumerate() {
        let blob = vec![u8::try_from(tag).expect("tag"); 4096];
        let root = cas.cas.tree(&[file("f", &blob, false)]);
        let at = lease_dir(&dir, &format!("failed-{tag}"));
        let lease_falls_back = matches!(error, CasError::Write(..));
        let kind = std::mem::discriminant(&error);
        cas.next(Then::Fail(error));
        let outcome = materialize_cached(&cas, &cache, &root, &at, &[]).await;
        assert_eq!(
            tmp_files(&dir),
            0,
            "case {tag}: the fill's file is left in tmp/"
        );
        assert!(!cache.holds(&key(&blob, false)), "case {tag}");
        if lease_falls_back {
            outcome.expect("the lease fetches for itself");
            assert_eq!(std::fs::read(at.join("f")).expect("read"), blob);
        } else {
            let Err(TreeError::Cas(got)) = outcome else {
                panic!("case {tag}: {outcome:?}");
            };
            assert_eq!(std::mem::discriminant(&got), kind, "case {tag}: {got:?}");
        }
    }
    stop(cache, &gates);
}

/// Catches a lease killed during its fill (its future dropped while the CAS is still
/// writing) leaving the half-written file in `tmp/`, where no budget sees it until a
/// restart, or holding the key's slot: the file is gone at once the lease is, and the
/// next lease fills the key as if nothing had happened.
#[tokio::test]
async fn a_lease_killed_during_its_fill_leaves_nothing_in_tmp() {
    let dir = scratch("cache-tmp-killed");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Halfway::default();
    let blob = vec![9_u8; 1 << 20];
    let root = cas.cas.tree(&[file("f", &blob, false)]);
    cas.next(Then::Stall);
    let killed = lease_dir(&dir, "killed");
    let run = materialize_cached(&cas, &cache, &root, &killed, &[]);
    let outcome = tokio::time::timeout(Duration::from_millis(300), run).await;
    assert!(outcome.is_err(), "the fill did not stall");
    assert!(
        tmp_empties(&dir).await,
        "the killed fill's file is left in tmp/"
    );
    let at = lease_dir(&dir, "next");
    materialize_cached(&cas, &cache, &root, &at, &[])
        .await
        .expect("the next lease");
    assert_eq!(std::fs::read(at.join("f")).expect("read"), blob);
    assert!(cache.holds(&key(&blob, false)));
    assert_eq!(cache.stats().bytes, blob.len() as u64);
    assert_eq!(tmp_files(&dir), 0);
    stop(cache, &gates);
}

/// Catches a checked file the cache could not keep (its rename into `blobs/` refused)
/// left in `tmp/` when its lease is killed before it copied it (the file is handed on
/// as a path no one removes): with the fill held just before its rename, the lease is
/// killed, the fill let go; the file is removed once the step ends, and nothing is
/// indexed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checked_file_whose_lease_is_gone_is_removed() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("cache-tmp-loose");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Arc::new(CountingCas::default());
    let bytes = b"no lease to copy it";
    let k = key(bytes, false);
    let root = cas.tree(&[file("a", bytes, false)]);
    let prefix = dir.join("cache/v1/blobs").join(k.prefix());
    let chmod = |mode| {
        std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    chmod(0o500);
    let held = gates.hold(|p| matches!(p, Point::Filled(_)));
    let at = lease_dir(&dir, "loose");
    let (lease_cas, lease_cache) = (Arc::clone(&cas), Arc::clone(&cache));
    let lease = tokio::spawn(async move {
        materialize_cached(&*lease_cas, &lease_cache, &root, &at, &[]).await
    });
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(held))
        .await
        .expect("reached");
    lease.abort();
    assert!(lease.await.is_err_and(|e| e.is_cancelled()));
    gates.release(held);
    let emptied = tmp_empties(&dir).await;
    chmod(0o700);
    assert!(emptied, "the checked file is left in tmp/");
    assert!(!cache.holds(&k));
    stop(cache, &gates);
}
