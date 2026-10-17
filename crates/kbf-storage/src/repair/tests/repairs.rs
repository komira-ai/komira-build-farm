//! [`Repairs`] over a disk that panics, and the ledger that tells no one.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_store::{FaultFile, FaultFs, Fs, FsFile};
use kbf_types::Digest;

use super::{digest, record};
use crate::StorageError;
use crate::repair::{NoLedger, Options, RepairLedger, RepairRecord, Repairs};
use crate::service::tests::fake::Mem;

/// What a [`HookFs`] runs, on the blocking thread, at each append to one of its files.
type Hook = Arc<dyn Fn() + Send + Sync>;

/// A [`FaultFs`] that runs a hook at each append: a panic (the failure of a blocking call
/// that a journal error does not describe), or a wait (a journal write that is in progress
/// when the test acts).
struct HookFs {
    inner: FaultFs,
    on_append: Hook,
}

struct HookFile {
    inner: FaultFile,
    on_append: Hook,
}

/// How long a test waits for something that must happen: a hang deadline, not a speed.
const HANG: Duration = Duration::from_secs(60);

/// The bound a test gives the store's answer; with a paused clock it is not waited out.
const PROBE: Duration = Duration::from_secs(10);

/// A journal write held in progress: the hook of a [`HookFs`] that, once armed, waits on the
/// blocking thread (with the journal locked) until released.
#[derive(Default)]
struct Hold {
    state: Mutex<HoldState>,
    changed: Condvar,
}

#[derive(Default)]
struct HoldState {
    armed: bool,
    entered: bool,
    released: bool,
}

impl Hold {
    fn hook(self: &Arc<Self>) -> Hook {
        let this = Arc::clone(self);
        Arc::new(move || this.wait())
    }

    fn wait(&self) {
        let mut state = self.state.lock().expect("lock");
        if !state.armed || state.released {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        let (_state, timeout) = self
            .changed
            .wait_timeout_while(state, HANG, |state| !state.released)
            .expect("lock");
        assert!(!timeout.timed_out(), "the held write was never released");
    }

    /// The next append waits.
    fn arm(&self) {
        self.state.lock().expect("lock").armed = true;
    }

    /// Resolves once an append is waiting.
    async fn entered(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let state = this.state.lock().expect("lock");
            let (_state, timeout) = this
                .changed
                .wait_timeout_while(state, HANG, |state| !state.entered)
                .expect("lock");
            assert!(!timeout.timed_out(), "no write was held");
        })
        .await
        .expect("waited");
    }

    /// The waiting append goes on, and no later one waits.
    fn release(&self) {
        self.state.lock().expect("lock").released = true;
        self.changed.notify_all();
    }
}

/// Releases a [`Hold`] when dropped, so a test that fails an assertion does not leave a
/// blocking thread waiting out [`HANG`].
struct ReleaseOnDrop(Arc<Hold>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl Fs for HookFs {
    type File = HookFile;

    fn list(&self) -> io::Result<Vec<String>> {
        self.inner.list()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read(name)
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        self.inner.file_len(name)
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(name, offset, len)
    }

    fn create(&self, name: &str) -> io::Result<HookFile> {
        Ok(HookFile {
            inner: self.inner.create(name)?,
            on_append: Arc::clone(&self.on_append),
        })
    }

    fn create_positioned(&self, name: &str) -> io::Result<HookFile> {
        Ok(HookFile {
            inner: self.inner.create_positioned(name)?,
            on_append: Arc::clone(&self.on_append),
        })
    }

    fn open_append(&self, name: &str) -> io::Result<HookFile> {
        Ok(HookFile {
            inner: self.inner.open_append(name)?,
            on_append: Arc::clone(&self.on_append),
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
}

impl FsFile for HookFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        (self.on_append)();
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_at(offset, bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

/// Catches a panic in the journal's blocking call taken for success or for a journal error
/// (it would then be told as "stopped" and so as a disk fault): the call fails INTERNAL, the
/// debt is not recorded, and the debts already owed can still be read.
#[tokio::test]
async fn a_panic_in_the_journal_fails_the_call_and_loses_no_debt() {
    let armed = Arc::new(AtomicBool::new(false));
    let fs = HookFs {
        inner: FaultFs::new(),
        on_append: Arc::new({
            let armed = Arc::clone(&armed);
            move || assert!(!armed.load(Ordering::SeqCst), "the disk panics")
        }),
    };
    let repairs = Repairs::open(fs, Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    armed.store(true, Ordering::SeqCst);
    let answer = repairs.owe(record(2)).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    // The same for settling a debt: the call fails, and the debt is still owed.
    let answer = repairs.settle(&digest(1)).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    let answer = repairs
        .settle_evicted(&digest(1), repairs.begun(), &Mem::default(), PROBE)
        .await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
}

/// Catches a ledger that tells no one being asked for something: both of its reports return
/// at once.
#[tokio::test]
async fn the_ledger_that_tells_no_one_returns_at_once() {
    NoLedger.owed(&record(1)).await;
    NoLedger.settled(&digest(1)).await;
}

/// Catches a debt dropped for an absent blob while a write of it is committing, or one that
/// began owing after the worker looked: `settle_evicted` leaves the debt while a write of the
/// digest is in flight (however many: two writes of the same blob, one finished), and when
/// any write began owing since `begun` was read; with neither, it settles and the journal
/// forgets it. Mutants: the in-flight test removed; the `begun` comparison removed; the
/// count of a digest's writes dropped to zero by the first.
#[tokio::test]
async fn an_evicted_debt_is_dropped_only_when_no_write_is_in_its_window() {
    let store = Mem::default();
    let repairs =
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open");
    let first = repairs.owe(record(1)).await.expect("owed");
    let second = repairs.owe(record(1)).await.expect("owed again");
    assert!(repairs.is_in_flight(&digest(1)));
    let begun = repairs.begun();
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    drop(first);
    assert!(
        repairs.is_in_flight(&digest(1)),
        "the second write is still committing"
    );
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    drop(second);
    assert!(!repairs.is_in_flight(&digest(1)));

    // A write of another blob began owing after `begun` was read: not now, but later.
    let other = repairs.owe(record(2)).await.expect("owed");
    drop(other);
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    let begun = repairs.begun();
    assert!(
        repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(2)]);
}

/// Catches a write that fails to journal its debt leaving its blob marked in flight for ever
/// (the worker would never look at the debt again): the journal fails, the call fails, and
/// the digest is not in flight.
#[tokio::test]
async fn a_debt_that_could_not_be_journalled_is_not_in_flight() {
    let fs = FaultFs::new();
    let repairs = Repairs::open(fs.clone(), Options::default(), Arc::new(NoLedger)).expect("open");
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    let answer = repairs.owe(record(1)).await;
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert!(!repairs.is_in_flight(&digest(1)));
}

/// Catches an abandoned write (its task aborted between the debt and the commit) leaving its
/// blob in flight: the `Pending` it held is dropped with it, so the worker may look at the
/// debt again.
#[tokio::test]
async fn an_abandoned_write_is_no_longer_in_flight() {
    let repairs = Arc::new(
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open"),
    );
    let write = tokio::spawn({
        let repairs = Arc::clone(&repairs);
        async move {
            let _pending = repairs.owe(record(3)).await.expect("owed");
            std::future::pending::<()>().await;
        }
    });
    while !repairs.is_in_flight(&digest(3)) {
        tokio::task::yield_now().await;
    }
    write.abort();
    assert!(write.await.expect_err("aborted").is_cancelled());
    assert!(!repairs.is_in_flight(&digest(3)));
}

/// Catches an evicted debt reported dropped when the journal could not write it: the call
/// fails UNAVAILABLE and the debt is still owed.
#[tokio::test]
async fn an_evicted_debt_whose_settling_cannot_be_written_stays_owed() {
    let fs = FaultFs::new();
    let repairs = Repairs::open(fs.clone(), Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    let answer = repairs
        .settle_evicted(&digest(1), repairs.begun(), &Mem::default(), PROBE)
        .await;
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
}

/// Catches a debt dropped for a blob the store holds (a write committed after the worker
/// read it absent), and one dropped when the store could not say: with the blob held, or the
/// store failing, `settle_evicted` leaves the debt; with the blob absent it drops it.
/// Mutants: the store not asked; its error read as "absent".
#[tokio::test]
async fn an_evicted_debt_stays_while_the_store_holds_the_blob_or_cannot_say() {
    let repairs =
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    let store = Mem::default();
    store
        .blobs
        .lock()
        .expect("lock")
        .insert(digest(1), bytes::Bytes::from_static(b"held"));
    let begun = repairs.begun();
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(1)]);

    *store.fail.lock().expect("lock") = Some(StorageError::Internal("the store".into()));
    let answer = repairs
        .settle_evicted(&digest(1), begun, &store, PROBE)
        .await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);

    store.fail.lock().expect("lock").take();
    store.blobs.lock().expect("lock").clear();
    assert!(
        repairs
            .settle_evicted(&digest(1), begun, &store, PROBE)
            .await
            .expect("asked")
    );
    assert!(repairs.open_records().is_empty());
}

/// A ledger that remembers what it was told was settled.
#[derive(Default)]
struct Told {
    settled: Mutex<Vec<Digest>>,
}

impl RepairLedger for Told {
    fn owed<'a>(&'a self, _: &'a RepairRecord) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn settled<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.settled.lock().expect("lock").push(*digest) })
    }
}

/// Catches a settle that is cancelled (the worker's stop) while its journal write is in
/// progress releasing the gate with the write still to land. The gate is what keeps a new
/// degraded write's debt from being journalled between the settle's look and its write:
/// a debt for a blob that is journalled and then erased by the late write is a blob with
/// one copy and no debt. The write of `owed` for a blob already owed is a no-op, so the
/// late `done` would take the debt of both writes with it. With the journal write held, the
/// settle's task is aborted: the gate must stay held, a new write of the same digest must
/// wait for it, and once the held write lands the new write's debt is owed.
///
/// The interleaving that loses the debt (the journal call of the cancelled settle running
/// after the new write's) needs a thread to be descheduled between two statements, which a
/// test cannot arrange; this pins the property that rules it out, and the order of the two
/// journal writes. The ledger is told the debt is settled all the same (the journal has
/// forgotten it, so the frontend must not keep it). Mutants: `settle_evicted` taking the
/// gate as a guard that its own future owns (the gate is free once the future is dropped);
/// the ledger told from the future after the journal call (never, when it is cancelled).
#[tokio::test]
async fn a_settle_cancelled_mid_write_keeps_the_gate_until_the_write_lands() {
    let hold = Arc::new(Hold::default());
    let _release = ReleaseOnDrop(Arc::clone(&hold));
    let fs = HookFs {
        inner: FaultFs::new(),
        on_append: hold.hook(),
    };
    let ledger = Arc::new(Told::default());
    let repairs = Arc::new(
        Repairs::open(
            fs,
            Options::default(),
            Arc::clone(&ledger) as Arc<dyn RepairLedger>,
        )
        .expect("open"),
    );
    drop(repairs.owe(record(1)).await.expect("owed"));

    hold.arm();
    let begun = repairs.begun();
    let settle = tokio::spawn({
        let repairs = Arc::clone(&repairs);
        async move {
            repairs
                .settle_evicted(&digest(1), begun, &Mem::default(), PROBE)
                .await
        }
    });
    hold.entered().await;
    settle.abort();
    assert!(settle.await.expect_err("cancelled").is_cancelled());
    assert!(
        repairs.gate_is_held(),
        "the write of the cancelled settle is still in progress, so the gate is held"
    );

    // A degraded write of the same blob arrives: it waits for the gate.
    let write = tokio::spawn({
        let repairs = Arc::clone(&repairs);
        async move { repairs.owe(record(1)).await }
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!write.is_finished(), "the write is behind the gate");

    hold.release();
    let pending = write.await.expect("joined").expect("owed");
    assert!(repairs.is_in_flight(&digest(1)));
    assert_eq!(
        repairs.open_records(),
        [record(1)],
        "the debt of the new write survives the cancelled settle"
    );
    drop(pending);
    assert!(!repairs.gate_is_held());
    let told = async {
        while ledger.settled.lock().expect("lock").is_empty() {
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(HANG, told)
        .await
        .expect("the ledger was told");
    assert_eq!(*ledger.settled.lock().expect("lock"), [digest(1)]);
}

/// Catches a store that never answers `find_missing` holding the gate for ever: every
/// degraded write's `owe` waits for the gate, so one hung look at the store stalls them all.
/// The look is bounded: `settle_evicted` answers UNAVAILABLE after the probe, the debt stays,
/// and the gate is free (a write that owes goes through). The clock is paused: the probe is
/// not waited out, and the outer bound is only there so that a look that is not bounded
/// fails the test instead of hanging it. Mutant: the look unbounded.
#[tokio::test(start_paused = true)]
async fn an_evicted_debt_whose_store_does_not_answer_stays_and_frees_the_gate() {
    let repairs =
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    let store = Mem::default();
    store.knobs.hang_find_missing.store(true, Ordering::SeqCst);
    let begun = repairs.begun();
    let answer = tokio::time::timeout(
        PROBE * 3,
        repairs.settle_evicted(&digest(1), begun, &store, PROBE),
    )
    .await
    .expect("the look at the store is bounded by the probe");
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert_eq!(store.knobs.asked.load(Ordering::SeqCst), 1);
    assert_eq!(repairs.open_records(), [record(1)]);
    assert!(!repairs.gate_is_held());
    drop(
        repairs
            .owe(record(2))
            .await
            .expect("a write is not stalled"),
    );
    assert_eq!(repairs.open_records(), [record(1), record(2)]);
}
