//! The drain sequence over a farm on a fake state store: the order of its steps, what each
//! one does and when it ends. Time is paused, so a wait of minutes costs nothing.

use futures::FutureExt as _;
use futures::future::{AbortHandle, AbortRegistration};
use kbf_front::{Backend, Cache, Described, Dispatch, Submission};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tonic::{Code, Status};

use super::*;
use crate::drain::Phase;
use crate::farm::{EpochPolicy, Farm, Reported};
use crate::health::Readiness;
use crate::state_store::StateStore;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// The two waits, in seconds.
fn timing(lb_wait: u64, deadline: u64) -> DrainTiming {
    DrainTiming {
        lb_wait: Duration::from_secs(lb_wait),
        deadline: Duration::from_secs(deadline),
    }
}

/// A serving frontend's parts, as the drain stands them down: a farm over a state store, a
/// drain with the listeners' stops, and the sequence over them.
struct Rig {
    backend: Backend,
    farm: Arc<Farm>,
    drain: Arc<Drain>,
    readiness: Arc<Readiness>,
    closing: kbf_front::Closing,
    /// Completes when the drain sends the REAPI listener GOAWAY.
    goaway: oneshot::Receiver<()>,
    worker: Stop,
    api: Stop,
    stand: Stand,
    sequence: Sequence,
    /// Kept so the sweeper is not aborted before it is told to be.
    _sweeping: AbortRegistration,
}

impl Rig {
    fn new(timing: DrainTiming) -> Self {
        let store = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        // A store opened for a writer holds a generation.
        let value = 7_u64.to_be_bytes().to_vec();
        let generation = Txn::new().put(kbf_state::GENERATION, value, Cond::Any);
        store.txn(generation).expect("store").expect("put");
        let backend = Backend::from(Arc::new(Cache::memory()));
        let farm = Farm::new(
            backend.clone(),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: Arc::new(store),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
        let farm = Arc::new(farm);
        let (closer, closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        let drain = Arc::new(drain);
        let (sweeper, sweeping) = AbortHandle::new_pair();
        let (worker, api) = (Stop::new(), Stop::new());
        let stand = Stand {
            drain: Arc::clone(&drain),
            farm: Arc::clone(&farm),
            worker: worker.clone(),
            sweeper,
            wait: Duration::from_secs(5),
        };
        let sequence = Sequence::new(stand.clone(), api.clone(), timing);
        Self {
            backend,
            farm,
            drain,
            readiness,
            closing,
            goaway,
            worker,
            api,
            stand,
            sequence,
            _sweeping: sweeping,
        }
    }

    /// The retire mode of this frontend, which holds the sequence, as the frontend keeps it.
    fn retire(&self) -> OnceLock<Retire> {
        let retire = OnceLock::new();
        let _ = retire.set(Retire::new(self.stand.clone(), self.sequence.clone()));
        retire
    }
}

/// A request to run.
fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// Queues `request` on `farm`.
fn submit(farm: &Farm, request: Request) -> Result<kbf_front::Ticket, Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
    })
}

/// Registers the node `node-a` on the rig's farm, as a daemon's first stream does.
fn register(rig: &Rig) -> Inbox {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    rig.farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    inbox
}

/// How many `Start`s are waiting in `inbox`.
fn starts(inbox: &mut Inbox) -> usize {
    let messages = std::iter::from_fn(|| inbox.try_recv().ok());
    let sent = |message: &Result<ServerMessage, Status>| {
        let message = message.as_ref().expect("no error").message.as_ref();
        matches!(message, Some(server_message::Message::Start(_)))
    };
    messages.filter(sent).count()
}

/// Asserts that `begin` says it was the first (a second call is not) and answers not ready
/// before it returns; that in the wait the frontend still places work on its node, refuses
/// nothing and sends no GOAWAY; that when the wait is over new Execute calls are refused,
/// the REAPI, worker and operator listeners are sent GOAWAY, and its node is placed nothing
/// more; and that an open execution keeps the drain going until it is gone, when the drain
/// ends, closing.
#[tokio::test(start_paused = true)]
async fn the_wait_serves_everything_and_the_end_of_it_refuses_and_sends_goaway() {
    let mut rig = Rig::new(timing(10, 900));
    let mut inbox = register(&rig);
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    assert!(!rig.sequence.begin(), "a second begin was a first");
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(rig.readiness.is_stopping(), "still ready after begin");
    tokio::time::sleep(Duration::from_millis(9_999)).await;
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(!rig.closing.draining(), "refused calls in the wait");
    assert!(rig.goaway.try_recv().is_err(), "GOAWAY in the wait");
    assert!(rig.worker.fired().now_or_never().is_none());
    assert!(rig.api.fired().now_or_never().is_none());
    let _served = submit(&rig.farm, request(1)).expect("queued in the wait");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the wait did not place the work");

    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert!(rig.closing.draining(), "new calls taken after the wait");
    assert_eq!(rig.goaway.try_recv(), Ok(()));
    assert!(rig.worker.fired().now_or_never().is_some());
    assert!(rig.api.fired().now_or_never().is_some());
    let _refused = submit(&rig.farm, request(2)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 0, "the end of the wait placed work");

    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "ended while open");
    drop(open);
    let ended = tokio::time::timeout(Duration::from_secs(10), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain ends once the execution is gone");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that with an execution that never ends, the drain waits the whole deadline and
/// the clients' grace and no more; that the worker's stream is then ended UNAVAILABLE, so
/// its daemon dials another frontend; and that the farm is fenced and takes no work.
#[tokio::test(start_paused = true)]
async fn at_the_deadline_the_streams_that_are_left_are_ended() {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.sequence_finished().await;

    let waited = began.elapsed();
    let least = Duration::from_secs(30) + CLOSE_GRACE;
    assert!(waited >= least, "ended early: {waited:?}");
    assert!(waited < Duration::from_secs(40), "ended late: {waited:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
    let ended: Vec<Code> = std::iter::from_fn(|| inbox.try_recv().ok())
        .filter_map(|message| message.err())
        .map(|status| status.code())
        .collect();
    assert_eq!(ended, [Code::Unavailable]);
    assert!(submit(&rig.farm, request(1)).is_err(), "still takes work");
}

/// Asserts that a second stop signal (`Drain::close`) skips the waits: with a wait of ten
/// minutes and a deadline of fifteen, and an execution that never ends, the drain is over at
/// once.
#[tokio::test(start_paused = true)]
async fn a_second_signal_skips_both_waits() {
    let rig = Rig::new(timing(600, 900));
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.close();
    rig.drain.sequence_finished().await;

    // Only the clients' grace, for the execution that was open, is waited.
    let most = CLOSE_GRACE + Duration::from_secs(1);
    assert!(began.elapsed() < most, "{:?}", began.elapsed());
}

/// Asserts that a drain with no execution open goes straight through: with a wait of zero
/// and no execution, the drain is over without waiting for a deadline of fifteen minutes.
#[tokio::test(start_paused = true)]
async fn a_drain_with_nothing_open_does_not_wait_for_the_deadline() {
    let rig = Rig::new(timing(0, 900));
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.sequence_finished().await;

    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that at a stop signal a frontend with no state store does the plain drain, at
/// once, with no sequence.
#[tokio::test(start_paused = true)]
async fn a_stop_without_a_state_store_drains_at_once() {
    let rig = Rig::new(timing(10, 900));
    let none = OnceLock::new();

    stopped(&rig.drain, &none, Duration::from_secs(1)).await;

    assert!(!rig.drain.is_sequenced());
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.closing.draining());
}

/// Asserts that at a stop signal a frontend with a state store begins the sequence (a second
/// beginning, as the endpoint's, is not a first), waits out the sequence and returns when it
/// is over.
#[tokio::test(start_paused = true)]
async fn a_stop_over_a_state_store_runs_the_sequence() {
    let rig = Rig::new(timing(3, 900));
    let retire = rig.retire();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::MAX).await;

    assert!(rig.drain.is_sequenced());
    assert!(
        began.elapsed() >= Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.readiness.is_stopping());
    let retire = retire.get().expect("set");
    assert!(!retire.drain(), "the endpoint began it a second time");
}

/// Asserts that the stop timeout (`--shutdown-timeout-secs`) bounds a sequence that a stop
/// signal began: an execution that never ends is closed when it runs out, long before the
/// deadline.
#[tokio::test(start_paused = true)]
async fn the_stop_timeout_closes_the_streams_of_a_sequence() {
    let rig = Rig::new(timing(0, 900));
    let retire = rig.retire();
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::from_secs(5)).await;

    let waited = began.elapsed();
    assert!(waited >= Duration::from_secs(5), "ended early: {waited:?}");
    assert!(waited < Duration::from_secs(60), "ended late: {waited:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a task of the sequence that panics still ends it: the drain is not over
/// while the task holds its guard, and is over once the task is gone.
#[tokio::test(start_paused = true)]
async fn a_sequence_task_that_panics_still_ends_the_sequence() {
    let rig = Rig::new(timing(0, 900));
    let ends = Ends(Arc::clone(&rig.drain));
    assert!(rig.drain.sequence_finished().now_or_never().is_none());

    let task = tokio::spawn(async move {
        let _ends = ends;
        panic!("a step of the sequence failed");
    });

    assert!(task.await.is_err(), "the task did not panic");
    rig.drain.sequence_finished().await;
}

/// Asserts that the defaults are the ones the flags document: ten seconds of wait and
/// fifteen minutes of deadline.
#[test]
fn the_defaults_are_ten_seconds_and_fifteen_minutes() {
    let timing = DrainTiming::default();
    assert_eq!(timing.lb_wait, Duration::from_secs(10));
    assert_eq!(timing.deadline, Duration::from_secs(900));
}
