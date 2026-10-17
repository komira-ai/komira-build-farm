//! A holder that stands down with requeues the state store has not taken yet.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use kbf_sched::DaemonInstance;
use kbf_types::WorkerId;
use tokio::sync::mpsc;

use super::*;
use crate::farm::{Outbound, Reported, StreamId};
use crate::handover::tests::{Rig, request, submit};

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        let bytes = self.0.lock().expect("lock").clone();
        String::from_utf8(bytes).expect("UTF-8 logs")
    }
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

const LONG: Duration = Duration::from_secs(600);

fn register(rig: &Rig, instance: &str, predecessor: &str, outbound: Outbound) -> StreamId {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    rig.farm.register(
        &node(),
        DaemonInstance::new(instance),
        DaemonInstance::new(predecessor),
        Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    )
}

fn keys(rig: &Rig, prefix: &str) -> Vec<String> {
    let rows = rig.store.scan_prefix(prefix).expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// A farm that holds one operation, leased to a node, whose lease the node's replacement
/// has just given up while the state store refused every write: the requeue is unsettled,
/// and the store still holds the stale lease and no queue row. Returns the operation's
/// name in the store.
async fn with_an_unsettled_requeue(rig: &Rig) -> String {
    let (outbound, inbox) = mpsc::unbounded_channel();
    register(rig, "daemon", "", outbound);
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(keys(rig, "lease/").len(), 1, "the operation is leased");
    let operation = keys(rig, "op/").remove(0).replace("op/", "");

    // The daemon's process is proven stopped, and the store refuses what follows.
    drop(inbox);
    rig.store.refuse_next.store(usize::MAX, Ordering::SeqCst);
    let (outbound, _kept) = mpsc::unbounded_channel();
    let stream = register(rig, "daemon-2", "daemon", outbound);
    // The new process lists no lease: the one the old process held is given up.
    assert!(rig.farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    rig.store.refuse_next.store(0, Ordering::SeqCst);
    assert_eq!(
        keys(rig, "lease/").len(),
        1,
        "the stale lease is still stored"
    );
    assert!(keys(rig, "queue/").is_empty(), "nothing requeued is stored");
    operation
}

/// Catches: a holder that stands down with requeues its state store refused, never
/// written (the successor would recover the operation holding a lease the farm gave up,
/// and wait out its grace). A store that refuses the write three times and then takes it
/// has the operation's queue row (and loss count) when the holder is released, and no
/// lease row; a successor over it recovers the operation queued, not leased.
#[tokio::test]
async fn a_requeue_the_store_refused_is_written_before_the_writer_role_is_given_up() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    rig.store.refuse_next.store(3, Ordering::SeqCst);
    let seen = Arc::new(Mutex::new(None));
    let during = Arc::clone(&seen);
    let store = Arc::clone(&rig.store);
    rig.store.on_demote(move || {
        let rows = |prefix: &str| store.scan_prefix(prefix).expect("scan").len();
        *during.lock().expect("lock") = Some((rows("queue/"), rows("lease/")));
    });

    rig.stand(Duration::from_secs(30))
        .run()
        .await
        .expect("released");

    assert_eq!(
        *seen.lock().expect("lock"),
        Some((1, 0)),
        "written before the role was given up"
    );
    assert_eq!(keys(&rig, "queue/"), [format!("queue/{operation}")]);
    assert_eq!(keys(&rig, "loss/"), [format!("loss/{operation}")]);
    assert!(keys(&rig, "lease/").is_empty());
    assert_eq!(rig.store.refuse_next.load(Ordering::SeqCst), 0);

    let successor = crate::farm::Farm::new(
        rig.backend.clone(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(crate::state_store::StateStore {
        store: rig.store.clone(),
        epoch: crate::farm::EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a successor over the store");
    assert_eq!(successor.queued_operations(), 1, "recovered queued");
}

/// Catches: a holder that waits for ever on a store that refuses (the handover would
/// never complete), or one that stays silent about what it left behind. A store that
/// refuses past the deadline: the holder is released all the same, the write was tried
/// more than once, and the log names the operation and how many are left.
#[tokio::test]
async fn a_requeue_the_store_refuses_past_the_deadline_is_logged_and_the_handover_completes() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    rig.store.refuse_next.store(usize::MAX, Ordering::SeqCst);
    let lines = Lines::default();
    let _logging = lines.subscribe();
    let before = rig.store.writes_tried.load(Ordering::SeqCst);

    let run = rig.stand(Duration::from_millis(100)).run().await;

    assert!(run.is_ok(), "the handover completes");
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 1);
    assert!(rig.store.writes_tried.load(Ordering::SeqCst) >= before + 2);
    assert!(keys(&rig, "queue/").is_empty());
    let log = lines.text();
    assert!(log.contains("ERROR"), "{log}");
    assert!(log.contains(&operation), "{log}");
}

/// Catches: a holder that goes on writing after a newer writer exists. A store that says
/// so (Fenced) on the first attempt is tried exactly once and the holder, whose deadline
/// is ten minutes, is released at once; and a farm that already learned it from an
/// earlier write tries none. Either way the log names the operation.
#[tokio::test]
async fn a_newer_writer_stops_the_flush_at_once() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    rig.store.newer_writer.store(true, Ordering::SeqCst);
    let lines = Lines::default();
    let _logging = lines.subscribe();
    let before = rig.store.writes_tried.load(Ordering::SeqCst);

    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).run()).await;

    assert!(run.expect("not waiting for the deadline").is_ok());
    assert_eq!(rig.store.writes_tried.load(Ordering::SeqCst), before + 1);
    assert!(lines.text().contains(&operation), "{}", lines.text());

    // Already known: the next stand down does not try at all.
    let rig = Rig::new();
    let _operation = with_an_unsettled_requeue(&rig).await;
    rig.store.newer_writer.store(true, Ordering::SeqCst);
    rig.farm.tick().await;
    let before = rig.store.writes_tried.load(Ordering::SeqCst);
    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).run()).await;
    assert!(run.expect("not waiting for the deadline").is_ok());
    assert_eq!(rig.store.writes_tried.load(Ordering::SeqCst), before);
}

/// Catches: a flush that works on a farm that has nothing to write, or that has no state
/// store at all (a holder with nothing unsettled must not wait out its deadline).
#[tokio::test]
async fn a_holder_with_nothing_unsettled_does_not_wait() {
    let rig = Rig::new();
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).run()).await;
    assert!(run.expect("no wait").is_ok());
}
