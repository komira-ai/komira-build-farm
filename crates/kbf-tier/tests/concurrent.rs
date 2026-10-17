//! Several callers on one open block at once: syncs that overlap a failing sync or a
//! block's first directory sync, and many ranges reserved, written and committed
//! from several threads.

mod support;

use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kbf_meta::Epoch;
use kbf_store::{FaultFile, FaultFs, Fs, FsFile};
use kbf_tier::{Synced, Tier, TierError, TierStart, Unsynced, WriteTier};

use support::{Index, TIER, bytes, open_tier, options};

/// How long a sync that must wait is given to (wrongly) finish. Only a defect can
/// finish it early, so a slow machine cannot turn a correct tier red.
const WAIT: Duration = Duration::from_millis(300);

/// One armed sync: it says it was reached, then blocks until the test sends what it
/// returns.
#[derive(Debug)]
struct Gate {
    reached: mpsc::Sender<()>,
    release: mpsc::Receiver<io::Result<()>>,
}

/// The gates of a [`GateFs`]: the next file sync and the next directory sync.
#[derive(Debug, Default)]
struct Gates {
    file_sync: Mutex<Option<Gate>>,
    dir_sync: Mutex<Option<Gate>>,
    dir_syncs: AtomicUsize,
}

/// Arms `slot`: the next sync through it blocks. Returns the "reached" receiver and
/// the "release" sender.
fn arm(slot: &Mutex<Option<Gate>>) -> (mpsc::Receiver<()>, mpsc::Sender<io::Result<()>>) {
    let (reached, reached_rx) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    *slot.lock().unwrap() = Some(Gate { reached, release });
    (reached_rx, release_tx)
}

/// Passes the gate armed in `slot`, if any: what the test released it with.
fn pass(slot: &Mutex<Option<Gate>>) -> io::Result<()> {
    let gate = slot.lock().unwrap().take();
    gate.map_or(Ok(()), |gate| {
        gate.reached.send(()).unwrap();
        gate.release.recv().unwrap()
    })
}

/// A [`FaultFs`] whose next file sync or directory sync can be held open by the test.
#[derive(Clone, Debug)]
struct GateFs {
    inner: FaultFs,
    gates: Arc<Gates>,
}

#[derive(Debug)]
struct GateFile {
    inner: FaultFile,
    gates: Arc<Gates>,
}

impl Fs for GateFs {
    type File = GateFile;

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

    fn create(&self, name: &str) -> io::Result<GateFile> {
        self.wrap(self.inner.create(name)?)
    }

    fn create_positioned(&self, name: &str) -> io::Result<GateFile> {
        self.wrap(self.inner.create_positioned(name)?)
    }

    fn open_append(&self, name: &str) -> io::Result<GateFile> {
        self.wrap(self.inner.open_append(name)?)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.gates.dir_syncs.fetch_add(1, Ordering::SeqCst);
        pass(&self.gates.dir_sync)?;
        self.inner.sync_dir()
    }
}

impl GateFs {
    #[allow(clippy::unnecessary_wraps, reason = "the shape of every opener")]
    fn wrap(&self, inner: FaultFile) -> io::Result<GateFile> {
        Ok(GateFile {
            inner,
            gates: Arc::clone(&self.gates),
        })
    }
}

impl FsFile for GateFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_at(offset, bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        pass(&self.gates.file_sync)?;
        self.inner.sync()
    }
}

/// A tier over a fresh [`GateFs`], one block of up to 1 MiB.
fn fresh() -> (Arc<Gates>, Arc<Tier<GateFs>>) {
    let gates = Arc::new(Gates::default());
    let fs = GateFs {
        inner: FaultFs::new(),
        gates: Arc::clone(&gates),
    };
    let seq = AtomicU64::new(0);
    let start = TierStart {
        tier: TIER,
        writer: kbf_meta::Writer::Epoch(Epoch::new(1)),
        options: options(1 << 30, 1 << 20),
    };
    let next = Box::new(move || seq.fetch_add(1, Ordering::Relaxed));
    let (tier, _) = Tier::open(fs, start, &Index::default(), next).unwrap();
    (gates, Arc::new(tier))
}

/// Syncs `r` on a new thread; its result arrives on the receiver.
fn sync_on_thread(
    tier: &Arc<Tier<GateFs>>,
    r: kbf_tier::Reservation,
) -> mpsc::Receiver<Result<Synced, Unsynced>> {
    let (tx, rx) = mpsc::channel();
    let tier = Arc::clone(tier);
    thread::spawn(move || tx.send(tier.sync(r)).unwrap());
    rx
}

/// Two written ranges of one fresh block.
fn two_ranges(tier: &Tier<GateFs>) -> (kbf_tier::Reservation, kbf_tier::Reservation) {
    let now = Instant::now();
    let a = tier.reserve_group(4, now).unwrap();
    let b = tier.reserve_group(4, now).unwrap();
    assert_eq!(a.object(), b.object());
    tier.write(&a, 0, b"aaaa").unwrap();
    tier.write(&b, 0, b"bbbb").unwrap();
    (a, b)
}

/// Waits `WAIT` for a sync that must not finish yet.
fn still_waiting(rx: &mpsc::Receiver<Result<Synced, Unsynced>>, why: &str) {
    match rx.recv_timeout(WAIT) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{why}: it returned {other:?}"),
    }
}

fn io_error() -> io::Result<()> {
    Err(io::Error::from_raw_os_error(5))
}

/// Catches: a sync that succeeds while another sync of the same block is failing.
/// All ranges of a block share one handle, and the kernel reports a write-back error
/// to one `fdatasync` of it: range A's failing sync can take the error of B's pages,
/// so B's sync returning 0 then proves nothing. Mutants: the per-block sync lock
/// removed (B's `fdatasync` returns at once, before A poisons the block: B is
/// acknowledged), and the poison taken after the lock is released.
#[test]
fn a_sync_never_succeeds_while_another_sync_of_its_block_is_failing() {
    let (gates, tier) = fresh();
    let (a, b) = two_ranges(&tier);
    let object = a.object();
    let (reached, release) = arm(&gates.file_sync);
    let a = sync_on_thread(&tier, a);
    reached.recv().unwrap();
    let b = sync_on_thread(&tier, b);
    still_waiting(
        &b,
        "range B's sync finished while A's sync of its block was failing",
    );
    release.send(io_error()).unwrap();
    let a = a.recv().unwrap().unwrap_err();
    assert!(matches!(a.error, TierError::Io { .. }), "{a:?}");
    let b = b.recv().unwrap().unwrap_err();
    assert!(
        matches!(b.error, TierError::Poisoned(o) if o == object),
        "{b:?}"
    );
}

/// Catches: a range acknowledged while the block's first directory sync (the one
/// that makes its name durable) is still running or has failed, and a block whose
/// name is synced more than once. Mutants: `dir_synced` set before `sync_dir` returns
/// together with the sync lock released before it (B sees the flag and returns
/// `Synced` while A's `sync_dir` is blocked); the sync lock released before
/// `sync_dir` alone (B runs a second `sync_dir`, counted).
#[test]
fn no_range_is_synced_before_its_block_name_is_durable() {
    let (gates, tier) = fresh();
    let (a, b) = two_ranges(&tier);
    let object = a.object();
    let (reached, release) = arm(&gates.dir_sync);
    let a = sync_on_thread(&tier, a);
    reached.recv().unwrap();
    let b = sync_on_thread(&tier, b);
    still_waiting(
        &b,
        "range B's sync finished while its block's name was not durable",
    );
    release.send(io_error()).unwrap();
    let a = a.recv().unwrap().unwrap_err();
    assert!(matches!(a.error, TierError::Io { .. }), "{a:?}");
    let b = b.recv().unwrap().unwrap_err();
    assert!(
        matches!(b.error, TierError::Poisoned(o) if o == object),
        "{b:?}"
    );

    // The same race whose directory sync succeeds: both ranges are synced, by one
    // directory sync. The failed block is poisoned, so these go to a new one.
    let (c, d) = two_ranges(&tier);
    assert_ne!(c.object(), object);
    let before = gates.dir_syncs.load(Ordering::SeqCst);
    let (reached, release) = arm(&gates.dir_sync);
    let c = sync_on_thread(&tier, c);
    reached.recv().unwrap();
    let d = sync_on_thread(&tier, d);
    still_waiting(
        &d,
        "range D's sync finished while its block's name was not durable",
    );
    release.send(Ok(())).unwrap();
    let c = c.recv().unwrap().unwrap();
    let d = d.recv().unwrap().unwrap();
    assert_eq!(gates.dir_syncs.load(Ordering::SeqCst) - before, 1);
    let (o, c_at, d_at) = (c.object(), c.offset(), d.offset());
    tier.committed(c);
    tier.committed(d);
    assert_eq!(tier.read(o, c_at, 4).unwrap(), b"aaaa");
    assert_eq!(tier.read(o, d_at, 4).unwrap(), b"bbbb");
}

/// Catches: concurrent reservations in one open block that overlap or leave a gap,
/// and acknowledged ranges that do not read back, live and after a power cut that
/// keeps no unsynced byte, when many callers reserve, write, sync and commit at once.
/// This is the many-writer form of the single-threaded tests; no planted mutant is
/// caught by it alone (the out-of-order commit mutant is caught deterministically by
/// `out_of_order_commits_never_move_the_committed_length_back`, not reliably here).
#[test]
fn concurrent_ranges_in_one_block_all_read_back_after_a_power_cut() {
    const THREADS: u8 = 8;
    const RANGES: u8 = 16;
    let fs = FaultFs::new();
    let (tier, _) = open_tier(&fs, 1, options(1 << 30, 1 << 20), &Index::default()).unwrap();
    let tier = Arc::new(tier);
    let index = Arc::new(Mutex::new(Index::default()));
    let now = Instant::now();
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let (tier, index) = (Arc::clone(&tier), Arc::clone(&index));
            thread::spawn(move || {
                (0..RANGES)
                    .map(|i| {
                        let tag = t * RANGES + i;
                        let data = bytes(tag, 1 + usize::from(tag % 7));
                        let r = tier.reserve_group(data.len() as u64, now).unwrap();
                        tier.write(&r, 0, &data).unwrap();
                        let synced = tier.sync(r).unwrap();
                        let at = (synced.object(), synced.offset(), data);
                        index.lock().unwrap().put_staged(&synced, false);
                        tier.committed(synced);
                        at
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut acked: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    acked.sort_by_key(|(o, offset, _)| (*o, *offset));
    let object = acked[0].0;
    let mut next = 0;
    for (o, offset, data) in &acked {
        assert_eq!(
            (*o, *offset),
            (object, next),
            "one block, no overlap, no gap"
        );
        next += data.len() as u64;
        assert_eq!(&tier.read(*o, *offset, data.len()).unwrap(), data);
    }
    let index = index.lock().unwrap().clone();
    let (tier, rec) = open_tier(&fs.crash(0), 2, options(1 << 30, 1 << 20), &index).unwrap();
    assert_eq!(rec.lost, [], "{rec:?}");
    for (o, offset, data) in &acked {
        assert_eq!(&tier.read(*o, *offset, data.len()).unwrap(), data);
    }
}
