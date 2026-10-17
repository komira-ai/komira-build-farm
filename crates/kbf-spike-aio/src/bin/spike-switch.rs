//! THROWAWAY SPIKE: the switch-time questions of docs/design/aio-handover.md section 12.2.
//!
//! `spike-switch --dir <scratch dir> [--runs N] [--sizes a,b,c] [--ops N] [--done N]`.
//! Prints evidence lines, asserts nothing.
//!
//! Components measured, each p50/p99/max over `--runs` repeats:
//! - meta log: `RaftMetaLog::open_dir` on a log of each size (the cost `i` pays at the cut
//!   without a follower open), the drop of the writer (`p`'s side), and the refused open
//!   while the writer is alive;
//! - state store: `Store::demote` on a SQLite file under a write load, the open of the
//!   file by the successor, and the scheduler recovery (`Farm::with_state`: epoch, load,
//!   recover) on a file holding `--ops` unfinished operations and `--done` finished ones.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, MetaLog, Submission};
use kbf_meta::{Command, Epoch, Location, ObjectId, Retention, StoreId};
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::Reported;
use kbf_server::{EpochPolicy, Farm, RaftMetaLog, StateStore};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Store, SystemClock};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;
use tonic::Code;

const GIB: u64 = 1 << 30;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `p50 / p99 / max` of `v` in milliseconds.
fn pct(v: &mut [Duration]) -> String {
    v.sort();
    let at = |q: f64| v[(((v.len() - 1) as f64) * q).round() as usize];
    format!("n={} p50={:.1}ms p99={:.1}ms max={:.1}ms", v.len(), ms(at(0.5)), ms(at(0.99)), ms(*v.last().expect("a sample")))
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

fn digest(n: u64) -> Digest {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    Digest::new(DigestFunction::Sha256, bytes, n)
}

// ---------------------------------------------------------------------------------------
// meta log

async fn meta(dir: &Path, sizes: &[u64], runs: usize, ticks: bool) {
    for &size in sizes {
        let kind = if ticks { "ticks" } else { "blobs" };
        let d = dir.join(format!("meta-{kind}-{size}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        let t = Instant::now();
        let log = RaftMetaLog::open_dir(&d, Retention::default(), "kbf/").await.expect("open");
        let epoch = match log.commit(Command::AllocEpoch).await.expect("epoch") {
            kbf_meta::Applied::Epoch(e) => e.get(),
            other => panic!("{other:?}"),
        };
        // `size` commands, committed 256 at a time (the committer groups them into one
        // batch and one sync); a segment commit names 32 blobs.
        let log = Arc::new(log);
        let mut done = 0u64;
        while done < size {
            let n = 256.min(size - done);
            let mut set = tokio::task::JoinSet::new();
            for i in 0..n {
                let log = Arc::clone(&log);
                let base = done + i;
                set.spawn(async move {
                    if ticks {
                        log.commit(Command::Tick(kbf_types::FarmTime::from_millis(1_000 + base * 1_000))).await.expect("tick");
                        return;
                    }
                    let blobs = (0..32)
                        .map(|k| {
                            (
                                digest(base * 32 + k),
                                Location {
                                    store: StoreId::new(0),
                                    object: ObjectId::new(Epoch::new(epoch), base + 1),
                                    offset: k * 4096,
                                },
                            )
                        })
                        .collect();
                    log.commit(Command::PutBlobs(blobs)).await.expect("commit");
                });
            }
            while set.join_next().await.is_some() {}
            done += n;
        }
        let built = t.elapsed();
        let applied = log.applied_index();
        // A second opener while the writer lives: refused, how fast.
        let t = Instant::now();
        let refused = RaftMetaLog::open_dir(&d, Retention::default(), "kbf/").await.err().map(|e| e.to_string());
        let refusal = t.elapsed();
        // `p` releases: the drop waits for the batch being written.
        let t = Instant::now();
        drop(Arc::into_inner(log).expect("sole owner"));
        let dropped = t.elapsed();
        println!(
            "meta[{kind}] size={size} cmds: built in {:.1}s, applied index {applied}, dir {} MiB; second open while held: {} in {:.2}ms; drop of the writer {:.2}ms",
            built.as_secs_f64(),
            dir_bytes(&d) >> 20,
            refused.map_or("OPENED (!)".to_owned(), |e| e.chars().take(40).collect()),
            ms(refusal),
            ms(dropped),
        );
        let mut opens = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            let log = RaftMetaLog::open_dir(&d, Retention::default(), "kbf/").await.expect("reopen");
            opens.push(t.elapsed());
            assert_eq!(log.applied_index() >= applied, true);
            drop(log);
        }
        println!("meta[{kind}] size={size}: open_dir (lock + replay of {applied} entries + elect) {}", pct(&mut opens));
        // The rate says what a follower would still owe for a tail of `n` entries if it
        // applied at the replay rate (a lower bound on the tail: no per-entry fsync).
        let p50 = {
            opens.sort();
            opens[opens.len() / 2]
        };
        println!("meta[{kind}] size={size}: replay rate ~{:.0} entries/s", applied as f64 / p50.as_secs_f64());
        // Reading the log without the lock: `open` over a copy of the directory stands for
        // the follower's initial catch-up (the same replay, no `flock`).
        let copy = dir.join(format!("meta-{kind}-{size}-copy"));
        let _ = std::fs::remove_dir_all(&copy);
        std::fs::create_dir_all(&copy).expect("dir");
        for e in std::fs::read_dir(&d).expect("ls").flatten() {
            if e.file_name() != kbf_server::raft_log::LOCK {
                std::fs::copy(e.path(), copy.join(e.file_name())).expect("copy");
            }
        }
        let fs = kbf_store::StdFs::new(&copy).expect("fs");
        let t = Instant::now();
        let log = RaftMetaLog::open(fs, Retention::default(), "kbf/", || unreachable!("has a record"))
            .await
            .expect("open copy");
        println!("meta[{kind}] size={size}: open without the lock over a copy {:.1}ms (applied {})", ms(t.elapsed()), log.applied_index());
        drop(log);
    }
}

// ---------------------------------------------------------------------------------------
// state store and scheduler

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

fn request(n: u64) -> Request {
    Request {
        key: ActionKey { instance: "main".to_owned(), action: digest(n) },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn submit(farm: &Farm, n: u64) {
    farm.submit(Submission { request: request(n), call: None, described: Described::default() })
        .map(drop)
        .expect("submitted");
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

fn register(farm: &Farm, cores: u64) -> Inbox {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps").with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(cores * 1_000, 64 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let _ = farm.register(&node(), DaemonInstance::new("daemon"), DaemonInstance::new(""), reported, outbound, ServerMessage::default());
    inbox
}

fn starts(inbox: &mut Inbox) -> Vec<LeaseId> {
    let mut leases = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(server_message::Message::Start(start)) = message.expect("no error").message {
            let l = start.lease_id.expect("a lease id");
            leases.push(LeaseId::new(l.term, l.seq));
        }
    }
    leases
}

fn invalid(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId { term: lease.term, seq: lease.seq }),
        status: Some(rpc::Status { code: Code::InvalidArgument as i32, message: "x".to_owned(), details: Vec::new() }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

fn open_sqlite(path: &Path) -> (Arc<SqliteStore>, Duration) {
    let t = Instant::now();
    let s = SqliteStore::open(path, Arc::new(SystemClock::default()), Options::default()).expect("sqlite");
    (Arc::new(s), t.elapsed())
}

fn farm_over(store: Arc<dyn Store>) -> (Farm, Duration) {
    let t = Instant::now();
    let farm = Farm::new(Arc::new(Cache::memory()), kbf_sched::UNSERVABLE_WAIT, kbf_sched::FINISHED_RETENTION)
        .with_state(Some(StateStore { store, epoch: EpochPolicy::Keep, members: Default::default(), history: None }))
        .expect("farm over state");
    (farm, t.elapsed())
}

async fn state(dir: &Path, ops: u64, done: u64, runs: usize) {
    let path = dir.join("state.db");
    for suffix in ["", "-wal", "-shm", ".lock"] {
        let _ = std::fs::remove_file(dir.join(format!("state.db{suffix}")));
    }
    let (store, t_open_new) = open_sqlite(&path);
    let (farm, _) = farm_over(store.clone());
    let mut inbox = register(&farm, 4);
    // Finished operations first (a `done/` row each), then the unfinished ones.
    let t = Instant::now();
    for n in 0..done {
        submit(&farm, 1_000_000 + n);
        let leases = starts(&mut inbox);
        for lease in leases {
            let ack = farm.report(&node(), invalid(lease)).await;
            assert!(ack.expect("ack").accepted);
        }
    }
    // 16 submitters, so the writer groups their commits (a busy farm's shape).
    std::thread::scope(|scope| {
        for t in 0..16u64 {
            let farm = &farm;
            scope.spawn(move || {
                let mut n = t;
                while n < ops {
                    submit(farm, n);
                    n += 16;
                }
            });
        }
    });
    let leases = starts(&mut inbox);
    println!(
        "state: built {done} done + {ops} unfinished ({} leased) in {:.1}s; file {} MiB (+wal {} MiB); first open of a new file {:.1}ms",
        leases.len(),
        t.elapsed().as_secs_f64(),
        std::fs::metadata(&path).map_or(0, |m| m.len()) >> 20,
        std::fs::metadata(dir.join("state.db-wal")).map_or(0, |m| m.len()) >> 20,
        ms(t_open_new)
    );
    drop(farm);
    drop(store);

    // Repeated cut: p holds the store under a write load, demotes it; i opens and recovers.
    let (mut demote_idle, mut demote_busy, mut open_after, mut recover, mut epoch_seen) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for run in 0..runs {
        let (store, _) = open_sqlite(&path);
        let (farm, t_recover_p) = farm_over(store.clone());
        let _ = t_recover_p;
        // Idle demote on every other run, under write load on the rest.
        let busy = run % 2 == 1;
        let stop = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicU64::new(0));
        let load = busy.then(|| {
            let (farm, stop, writes) = (Arc::new(farm), Arc::clone(&stop), Arc::clone(&writes));
            let f2 = Arc::clone(&farm);
            let h = std::thread::spawn(move || {
                let mut n = 10_000_000u64 + run as u64 * 1_000_000;
                while !stop.load(Ordering::SeqCst) {
                    // After the demote a submit is refused: the loop only counts accepted ones.
                    let r = farm.submit(Submission { request: request(n), call: None, described: Described::default() });
                    if r.is_ok() {
                        writes.fetch_add(1, Ordering::SeqCst);
                    }
                    n += 1;
                }
            });
            (h, f2)
        });
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        let dyn_store: Arc<dyn Store> = store.clone();
        let demoted = dyn_store.demote();
        let took = t.elapsed();
        assert!(demoted.is_ok(), "{demoted:?}");
        if busy {
            demote_busy.push(took);
        } else {
            demote_idle.push(took);
        }
        stop.store(true, Ordering::SeqCst);
        if let Some((h, farm)) = load {
            h.join().expect("load");
            drop(farm);
        }
        // The lock goes with the demote: the successor can open at once.
        drop(dyn_store);
        let t = Instant::now();
        let (opened, _) = {
            let mut tries = 0;
            loop {
                match SqliteStore::open(&path, Arc::new(SystemClock::default()), Options::default()) {
                    Ok(s) => break (Arc::new(s), tries),
                    Err(e) if tries < 200 => {
                        let _ = e;
                        tries += 1;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("successor could not open: {e}"),
                }
            }
        };
        open_after.push(t.elapsed());
        // Recovery on the successor, while the predecessor still holds its (demoted) handle.
        let (farm_i, took) = farm_over(opened.clone());
        recover.push(took);
        epoch_seen.push(farm_i.lease_epoch());
        drop(farm_i);
        drop(opened);
        drop(store);
    }
    println!("state: Store::demote, idle  {}", pct(&mut demote_idle));
    println!("state: Store::demote, under submit load {}", pct(&mut demote_busy));
    println!("state: successor SqliteStore::open after the demote {}", pct(&mut open_after));
    println!("state: scheduler recovery (Farm::with_state: epoch + load + recover) {}", pct(&mut recover));
    epoch_seen.dedup();
    println!("state: lease_epoch across the repeated handovers (Keep): {epoch_seen:?}");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str, default: &str| {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_owned())
    };
    let dir = PathBuf::from(get("--dir", ""));
    assert!(dir.is_dir(), "--dir <scratch dir>");
    let runs: usize = get("--runs", "15").parse().expect("runs");
    let sizes: Vec<u64> = get("--sizes", "10000,100000").split(',').map(|s| s.parse().expect("size")).collect();
    let ops: u64 = get("--ops", "20000").parse().expect("ops");
    let done: u64 = get("--done", "2000").parse().expect("done");
    let what = get("--what", "meta,state");
    for part in what.split(',') {
        match part {
            "meta" => meta(&dir, &sizes, runs, false).await,
            "ticks" => meta(&dir, &sizes, runs, true).await,
            "state" => state(&dir, ops, done, runs).await,
            other => panic!("unknown part {other}"),
        }
    }
}
