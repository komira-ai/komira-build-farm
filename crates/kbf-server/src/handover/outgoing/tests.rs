//! A frontend that serves, asked to hand over by successors the tests play.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use kbf_front::Stage;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::*;
use crate::handover::tests::{Rig, request, submit};
use crate::handover::wire::WireError;

const LONG: Duration = Duration::from_secs(600);

fn hello(rig: &Rig, change: impl FnOnce(&mut Message)) -> Message {
    let mut hello = Message::Hello {
        version: PROTOCOL,
        reapi: rig.facts.reapi,
        worker: rig.facts.worker,
        api: rig.facts.api,
        state: rig.facts.state.clone(),
    };
    change(&mut hello);
    hello
}

async fn connect(rig: &Rig) -> Link {
    Link::new(
        UnixStream::connect(rig.dir.socket())
            .await
            .expect("connect"),
    )
}

/// Plays a successor to the end: hello, takes the offer, ready, and what comes back.
async fn take_over(rig: &Rig) -> Result<Message, WireError> {
    let mut link = connect(rig).await;
    link.say(&hello(rig, |_| {})).await.expect("hello");
    let (offer, sockets) = link.hear_with().await?;
    assert!(matches!(offer, Message::Offer { .. }), "{offer:?}");
    assert_eq!(sockets.len(), 3);
    link.say(&Message::Ready).await.expect("ready");
    link.hear().await
}

/// Whether the rig's frontend has been touched: its drain begun.
fn untouched(rig: &Rig) -> bool {
    !rig.drain.is_draining()
}

/// Catches: a holder that stands down in the wrong order, or leaves a step out. When the
/// writer role is given up, everything before it is done (REAPI stopped and draining, the
/// farm fenced, the sweeper aborted, the worker and API listeners stopped, the daemons'
/// streams ended), and the successor is told only after, with the generation of the
/// store and sockets that are the holder's own. A holder that fences after giving up the
/// store, or lets the daemons' streams run on, fails here.
#[tokio::test]
async fn a_holder_stands_down_in_order_and_says_released() {
    let rig = Rig::new();
    let seen = Arc::new(Mutex::new(None));
    let during = Arc::clone(&seen);
    let (drain, worker, api, sweeper, farm) = (
        Arc::clone(&rig.drain),
        rig.worker.clone(),
        rig.api.clone(),
        rig.sweeper.clone(),
        Arc::clone(&rig.farm),
    );
    rig.store.on_demote(move || {
        let fenced = submit(&farm, request(9)).is_err_and(|e| e.code() == tonic::Code::Unavailable);
        *during.lock().expect("lock") = Some([
            drain.is_draining(),
            fenced,
            sweeper.is_aborted(),
            worker.fired().now_or_never().is_some(),
            api.fired().now_or_never().is_some(),
        ]);
    });
    let (outbound, mut inbox) = mpsc::unbounded_channel();
    let node = kbf_types::WorkerId::new("node-a");
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    rig.farm.register(
        &node,
        kbf_sched::DaemonInstance::new("daemon"),
        kbf_sched::DaemonInstance::new(""),
        crate::farm::Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    assert!(inbox.try_recv().is_ok(), "the Welcome");

    let holder = rig.holder(Rig::uid(), LONG);
    let served = tokio::spawn(holder.run(Arc::clone(&rig.farm), Duration::from_millis(1)));
    let mut link = connect(&rig).await;
    link.say(&hello(&rig, |_| {})).await.expect("hello");
    let (offer, sockets) = link.hear_with().await.expect("the offer");
    assert_eq!(
        offer,
        Message::Offer {
            generation: 7,
            api: true
        }
    );
    let offered: Vec<SocketAddr> = sockets
        .into_iter()
        .map(|fd| {
            std::net::TcpListener::from(fd)
                .local_addr()
                .expect("a listener")
        })
        .collect();
    assert_eq!(offered, [rig.addr(0), rig.addr(1), rig.addr(2)]);
    assert!(untouched(&rig), "the holder moved before ready");
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 0);

    link.say(&Message::Ready).await.expect("ready");
    assert_eq!(link.hear().await.expect("released"), Message::Released);
    assert_eq!(*seen.lock().expect("lock"), Some([true; 5]));
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 1);
    let ended = inbox
        .try_recv()
        .expect("the stream is ended")
        .expect_err("with a status");
    assert_eq!(ended.code(), tonic::Code::Unavailable);
    served
        .await
        .expect("the holder goes on to answer its callers, and has none");
}

/// Catches: a holder that hands over to a process started with other flags (it would
/// serve listeners it was not given, or open another state store), or to one that speaks
/// another protocol or opens with something else than a hello, and a holder that stops
/// accepting successors, or stands down, after refusing one. Each refusal names why and
/// leaves the frontend untouched, and a good successor still takes over after them all.
#[tokio::test]
async fn a_successor_started_with_other_flags_is_refused_and_nothing_changes() {
    let rig = Rig::new();
    let served = tokio::spawn(
        rig.holder(Rig::uid(), LONG)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );
    let other: SocketAddr = "127.0.0.1:9".parse().expect("address");
    let cases: Vec<(Message, &str)> = vec![
        (
            hello(&rig, |h| {
                if let Message::Hello { version, .. } = h {
                    *version = 2
                }
            }),
            "protocol 1, not 2",
        ),
        (
            hello(&rig, |h| {
                if let Message::Hello { reapi, .. } = h {
                    *reapi = other
                }
            }),
            "listen flags",
        ),
        (
            hello(&rig, |h| {
                if let Message::Hello { worker, .. } = h {
                    *worker = other
                }
            }),
            "listen flags",
        ),
        (
            hello(&rig, |h| {
                if let Message::Hello { api, .. } = h {
                    *api = None
                }
            }),
            "listen flags",
        ),
        (
            hello(&rig, |h| {
                if let Message::Hello { state, .. } = h {
                    *state = PathBuf::from("/other.db")
                }
            }),
            "--state-store",
        ),
        (Message::Ready, "must be hello"),
    ];
    for (message, why) in cases {
        let mut link = connect(&rig).await;
        link.say(&message).await.expect("say");
        match link.hear().await.expect("an answer") {
            Message::Refused(said) => assert!(said.contains(why), "{said}"),
            other => panic!("{message:?} was answered {other:?}"),
        }
        assert!(untouched(&rig), "{message:?}");
    }
    assert_eq!(
        take_over(&rig).await.expect("a good successor"),
        Message::Released
    );
    served.await.expect("the holder");
}

/// Catches: a handover that is half done when its successor fails or goes away before
/// `ready` (the frontend would be draining with nobody to serve): a successor that
/// closes after its hello, after the offer, says something else than `ready`, or sends
/// what is not a message. In each the frontend is untouched, and the next successor takes
/// over.
#[tokio::test]
async fn a_successor_that_fails_before_ready_changes_nothing() {
    let rig = Rig::new();
    let served = tokio::spawn(
        rig.holder(Rig::uid(), LONG)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );

    let mut link = connect(&rig).await;
    link.say(&hello(&rig, |_| {})).await.expect("hello");
    drop(link);

    let mut link = connect(&rig).await;
    link.say(&hello(&rig, |_| {})).await.expect("hello");
    link.hear_with().await.expect("the offer");
    drop(link);

    let mut link = connect(&rig).await;
    link.say(&hello(&rig, |_| {})).await.expect("hello");
    link.hear_with().await.expect("the offer");
    link.say(&Message::Released).await.expect("something else");

    let mut link = connect(&rig).await;
    link.say_raw(b"not a message\n").await;

    assert_eq!(
        take_over(&rig).await.expect("a good successor"),
        Message::Released
    );
    assert!(
        rig.store.demoted.load(Ordering::SeqCst) == 1,
        "only the good one stood the frontend down"
    );
    served.await.expect("the holder");
}

/// Catches: a control socket that serves another user (any local process could take the
/// frontend's listeners): a peer that is not the user the holder runs as is closed on
/// without an answer, however many come.
#[tokio::test]
async fn a_peer_that_is_another_user_is_not_answered() {
    let rig = Rig::new();
    let served = tokio::spawn(
        rig.holder(Rig::uid() + 1, LONG)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );
    for _ in 0..2 {
        let mut link = connect(&rig).await;
        let _ = link.say(&hello(&rig, |_| {})).await;
        let closed = link.hear().await.expect_err("no answer");
        assert!(
            matches!(closed, WireError::Closed | WireError::Io(_)),
            "{closed}"
        );
        assert!(untouched(&rig));
    }
    served.abort();
}

/// Catches: a holder that gives up the writer role while a call is between its request
/// and its queueing, or a result is being taken (the result could be lost, or the call
/// answered by neither process), and one that waits for ever for such a call: it stands
/// down only once the farm is quiet, or the wait is up. The farm still takes work until
/// then.
#[tokio::test]
async fn a_holder_waits_for_the_farm_to_be_quiet_for_a_bounded_time() {
    let rig = Rig::new();
    let busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    let served = tokio::spawn(
        rig.holder(Rig::uid(), LONG)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );
    let waiting = tokio::spawn({
        let rig_drain = Arc::clone(&rig.drain);
        async move {
            while !rig_drain.is_draining() {
                tokio::task::yield_now().await;
            }
        }
    });
    let mut link = connect(&rig).await;
    link.say(&hello(&rig, |_| {})).await.expect("hello");
    link.hear_with().await.expect("the offer");
    link.say(&Message::Ready).await.expect("ready");
    waiting.await.expect("the stand down began");
    assert!(
        submit(&rig.farm, request(1)).is_ok(),
        "fenced while a call was in flight"
    );
    assert_eq!(rig.store.demoted.load(Ordering::SeqCst), 0);
    drop(busy);
    assert_eq!(link.hear().await.expect("released"), Message::Released);
    assert!(submit(&rig.farm, request(2)).is_err());
    // The call it kept open has a caller waiting, so it goes on following.
    served.abort();

    // A wait that is up stands down with the farm still busy.
    let rig = Rig::new();
    let _busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    let served = tokio::spawn(
        rig.holder(Rig::uid(), Duration::ZERO)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );
    assert_eq!(take_over(&rig).await.expect("released"), Message::Released);
    served.await.expect("the holder");
}

/// Catches: a holder that cannot give up the writer role and says it did (the successor
/// would open a store the holder still writes), or goes on serving half stood down: the
/// successor is told `failed` with the reason, the drain is closed so the frontend ends
/// its streams and exits, and the control socket's task ends.
#[tokio::test]
async fn a_holder_that_cannot_give_up_the_store_says_failed_and_closes_its_drain() {
    let rig = Rig::new();
    rig.store.refuses_to_demote.store(true, Ordering::SeqCst);
    let served = tokio::spawn(
        rig.holder(Rig::uid(), LONG)
            .run(Arc::clone(&rig.farm), Duration::from_millis(1)),
    );
    match take_over(&rig).await.expect("an answer") {
        Message::Failed(why) => assert!(why.contains("the writer cannot stop"), "{why}"),
        other => panic!("{other:?}"),
    }
    served.await.expect("the task ends");
    rig.drain.closed().await;
}

/// Catches: a control socket that cannot replace the one a dead frontend left (the next
/// start would fail with "address in use"), is readable by other users, or that is made
/// where it cannot be.
#[tokio::test]
async fn the_control_socket_replaces_a_stale_one_and_is_for_this_user() {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    let rig = Rig::new();
    let path = rig.dir.socket();
    std::fs::write(&path, b"left behind").expect("a stale file");
    let _holder = rig.holder(Rig::uid(), LONG);
    let meta = std::fs::metadata(&path).expect("the socket");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let nowhere = rig.dir.0.join("missing").join("control.sock");
    let error = Holder::bind(
        &nowhere,
        Rig::uid(),
        rig.facts.clone(),
        rig.sockets(),
        rig.stand(LONG),
    )
    .err()
    .expect("no directory");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// Catches: an offer that names the wrong generation: the store's own, 0 for a store
/// that has none, and 0 (not a panic) for a row that is not a number.
#[tokio::test]
async fn the_offered_generation_is_the_stores() {
    let rig = Rig::new();
    let holder = rig.holder(Rig::uid(), LONG);
    assert_eq!(holder.generation().expect("generation"), 7);
    rig.store.put(kbf_state::GENERATION, vec![1, 2, 3]);
    assert_eq!(holder.generation().expect("generation"), 0);
    rig.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(holder.generation().is_err());
}

/// Catches: a follower that gives up while callers still wait (their streams hang, or
/// end), that does not read the store again after it failed to (one bad read ends the
/// answers), or that goes on after the last caller is answered (the process would never
/// finish). A caller whose operation is queued waits; the successor's `done` row answers
/// it; a failing read is retried.
#[tokio::test]
async fn the_follower_answers_from_the_store_until_no_caller_waits() {
    let rig = Rig::new();
    let mut ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.fence();
    let farm = Arc::clone(&rig.farm);
    let following = tokio::spawn(async move { follow(&farm, Duration::from_millis(1)).await });

    rig.store.reads_fail.store(true, Ordering::SeqCst);
    while rig.store.reads_failed.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    rig.store.reads_fail.store(false, Ordering::SeqCst);
    // A whole look (three reads) finds the caller still waiting before the answer is written.
    let served = rig.store.reads_served.load(Ordering::SeqCst);
    while rig.store.reads_served.load(Ordering::SeqCst) < served + 3 {
        tokio::task::yield_now().await;
    }
    assert!(
        !matches!(&*ticket.stage.borrow(), Stage::Done(_)),
        "nothing to say yet"
    );
    assert!(!following.is_finished());

    let finished = kbf_front::Finished::Failed(kbf_proto::google::rpc::Status {
        code: 9,
        message: "the successor's answer".to_owned(),
        details: Vec::new(),
    });
    let done = format!("done/{:016x}", 0);
    rig.store.put(&done, finished.to_row());
    let answered = ticket
        .stage
        .wait_for(|stage| matches!(stage, Stage::Done(_)))
        .await
        .expect("the stage changes");
    assert_eq!(*answered, Stage::Done(finished));
    drop(answered);
    following
        .await
        .expect("the follower ends with the last caller");
}
