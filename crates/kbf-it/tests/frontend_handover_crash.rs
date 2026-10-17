//! The frontend handover under crashes, with real processes: a `kbf storage` node and
//! `kbf frontend` processes (the old one, the new one and its restarts) sharing a state
//! file and a control socket, a daemon and a client over gRPC, and the state file read
//! back. The in-process tests of the handover (crates/kbf-server/tests/frontend) pin the
//! protocol; these pin what only a process death shows: a killed process gives up its
//! file lock, its sockets and its connections, and no one else does.
//!
//! No test orders anything by sleeping: a process is waited for by its start line, its
//! log or its exit; the timeouts are hang guards. Where a window cannot be hit by racing
//! two real processes (an old frontend that has offered its sockets and not yet been
//! told to stand down), the test plays the other end of the control socket itself.

#[path = "frontend_handover_crash/support.rs"]
mod support;

use std::os::unix::net::UnixListener;
use std::time::Duration;

use kbf_proto::worker::LeaseId;
use kbf_server::handover::Message;
use support::{
    Addrs, Client, Daemon, Frontend, Job, Rig, Rows, Wire, answered_with, done_once,
    execute_with_retry, first, hello,
};
use tonic::Code;

/// What a build in flight looks like before the update: a frontend serving, a daemon
/// connected, an Execute running on the daemon, the Execute's stream open.
struct Build {
    old: Frontend,
    client: Client,
    daemon: Daemon,
    job: Job,
    ops: tonic::Streaming<kbf_proto::google::longrunning::Operation>,
    name: String,
    lease: LeaseId,
}

impl Build {
    async fn start(rig: &Rig) -> Self {
        let old = rig.frontend("old", None);
        let client = Client::connect(old.addrs.reapi).await;
        let mut daemon = Daemon::connect(old.addrs.worker, "node-a").await;
        let job = Job::new("a build across the update");
        client.upload_job(&job).await;
        let mut ops = client.execute(&job).await.expect("Execute");
        let name = first(&mut ops).await.name;
        let lease = daemon.start().await.lease_id.expect("a lease id");
        Self {
            old,
            client,
            daemon,
            job,
            ops,
            name,
            lease,
        }
    }
}

/// Catches: a frontend killed after it offered its sockets and before the successor was
/// ready (a crash in the one window where the successor may already hold the sockets),
/// leaving the cluster unable to restart or the state damaged. The test is the successor
/// up to `offer` (it takes the three sockets, says nothing more), kills the old process
/// there, lets go of the sockets, and starts a real frontend. It must find the control
/// socket stale, start as any cold start does on the same addresses, take generation
/// + 1 and bump the epoch, keep every row of the operation that was queued and leased
/// (none lost, no id reused), and run that operation once when the daemon, which sees
/// the new epoch, lists no runs. Mutants: `Holder::bind` not replacing a stale socket
/// file (the restart fails on the control socket); the state store not being locked by
/// the old process alone (a second writer is not the case here, so the generation and
/// epoch checks carry it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_frontend_killed_after_its_offer_is_replaced_by_a_cold_start_that_loses_nothing() {
    let rig = Rig::start("old_killed_after_offer");
    let Build {
        mut old,
        client,
        daemon,
        job,
        ops,
        lease,
        ..
    } = Build::start(&rig).await;
    let before = Rows::read(&rig.state_db());
    assert_eq!(before.keys("op/").len(), 1, "{:?}", before.keys(""));
    assert_eq!(before.keys("lease/").len(), 1);

    // The successor, as far as `offer`.
    let mut wire = Wire::connect(&rig.control_socket());
    wire.say(&hello(&old.addrs, &rig.state_db()));
    let (offer, sockets) = wire.hear();
    assert_eq!(
        offer,
        Message::Offer {
            generation: before.generation(),
            api: true
        }
    );
    assert_eq!(sockets.len(), 3);
    // The old frontend waits for `ready`. It dies instead, and the successor with it.
    assert!(!old.proc.kill().success());
    drop((sockets, wire, daemon, ops, client));
    assert_eq!(
        Rows::read(&rig.state_db()),
        before,
        "a frontend killed while it offered changed the state"
    );

    let new = rig.frontend("new", Some(old.addrs));
    assert_eq!(
        new.addrs, old.addrs,
        "the restart serves the same addresses"
    );
    let after = Rows::read(&rig.state_db());
    assert_eq!(after.generation(), before.generation() + 1);
    assert_eq!(
        after.epoch(),
        before.epoch() + 1,
        "a cold start bumps the epoch"
    );
    assert_eq!(
        after.keys("op/"),
        before.keys("op/"),
        "the queued operation is kept"
    );
    assert!(after.number("meta/next_op") >= before.number("meta/next_op"));

    // The daemon sees another epoch, so it lists no runs; the operation runs again, once.
    let mut daemon = Daemon::connect(new.addrs.worker, "node-a").await;
    assert_ne!(daemon.epoch, lease.term, "the lease epoch changed");
    daemon.heartbeat(&[]).await;
    let rerun = daemon.start().await.lease_id.expect("a lease id");
    assert_ne!(rerun, lease);
    let client = Client::connect(new.addrs.reapi).await;
    let result = client.output("built after the crash").await;
    assert!(daemon.report(rerun, &result).await.accepted);
    let mut again = client.execute(&job).await.expect("Execute");
    let response = done_once(&mut again).await;
    assert!(answered_with(&response, &result), "{response:?}");
    let last = Rows::read(&rig.state_db());
    assert_eq!(last.keys("op/"), Vec::<String>::new());
    assert_eq!(last.keys("done/").len(), 1, "{:?}", last.keys(""));

    // A job submitted now gets an id no earlier one had.
    let other = Job::new("submitted after the restart");
    client.upload_job(&other).await;
    let _ops = client.execute(&other).await.expect("Execute");
    let latest = Rows::read(&rig.state_db());
    let fresh: Vec<_> = latest.keys("op/");
    assert_eq!(fresh.len(), 1);
    assert!(
        !before.keys("op/").contains(&fresh[0]) && fresh[0] > before.keys("op/")[0],
        "an operation id was reused: {fresh:?} after {:?}",
        before.keys("op/")
    );
}

/// Catches: a successor that opens the state store at a generation other than the one
/// the frontend it takes over from reported (a restored or swapped file, or another
/// writer that got there first), which would make two writers or a rollback. The test is
/// the frontend serving, up to `released`, and says the store is five generations past
/// where it is; the real successor must exit 2 saying so, print no start line, and leave
/// every row as it was. Mutant: `open_after_handover` not passing the expected generation
/// on (the successor takes generation + 1 and serves).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_told_another_generation_exits_2_and_writes_nothing() {
    let rig = Rig::start("generation_fence");
    // A state file with some history, and no frontend serving it.
    let mut first_run = rig.frontend("first", None);
    let addrs = first_run.addrs;
    let client = Client::connect(addrs.reapi).await;
    let job = Job::new("history");
    client.upload_job(&job).await;
    drop(client.execute(&job).await.expect("Execute"));
    drop(client);
    assert!(first_run.proc.term().success());
    let before = Rows::read(&rig.state_db());

    // The addresses and the control socket are the fake's now.
    let _ = std::fs::remove_file(rig.control_socket());
    let control = UnixListener::bind(rig.control_socket()).expect("bind the control socket");
    let (listeners, addrs) = Addrs::free();
    let state = rig.state_db();
    let wrong = before.generation() + 5;
    let (done, said) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = control.accept().expect("the successor connects");
        let mut wire = Wire(stream);
        let (hello, _) = wire.hear();
        wire.say_with(
            &Message::Offer {
                generation: wrong,
                api: true,
            },
            &listeners,
        );
        let (ready, _) = wire.hear();
        wire.say(&Message::Released);
        let _ = done.send((hello, ready));
        // Hold the listeners until the test is over with the successor.
        std::thread::park_timeout(Duration::from_secs(60));
        drop(listeners);
    });

    let mut successor = rig.spawn_frontend("successor", Some(addrs));
    let status = successor
        .start_line()
        .expect_err("the successor served at a generation it was not told");
    assert_eq!(status.code(), Some(2), "{}", successor.log());
    let (hello, ready) = said
        .recv_timeout(support::GUARD)
        .expect("the fake holder ran");
    assert_eq!(hello, support::hello(&addrs, &state));
    assert_eq!(ready, Message::Ready);
    let log = successor.log();
    assert!(
        log.is_empty() || !log.contains("serving"),
        "unexpected log: {log}"
    );
    assert_eq!(Rows::read(&rig.state_db()), before, "the store was written");
}

/// Catches: a new frontend killed after the old one let go that leaves the cluster without
/// a frontend, or lets the old one write again. The old process stays up as a follower
/// (its client's Execute stream is open); the new one is killed; a restart of the new
/// one on the same addresses (tried again only while the old process still holds a copy of
/// a listener) must open the store, which only a demoted old process lets go of, take
/// generation + 2 and a bumped epoch, and serve a daemon that reconnects, which runs the
/// operation again; the old process, which kept the client's stream, answers it with that
/// result and exits 0 once the client is gone. Mutants: the stand down not demoting the
/// state store (the restart cannot lock the file), not stopping the worker listener (the
/// restart never gets its address).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_frontend_killed_after_the_release_is_restarted_and_the_old_one_never_writes() {
    let rig = Rig::start("new_killed_after_release");
    let Build {
        mut old,
        client,
        mut daemon,
        mut ops,
        lease,
        ..
    } = Build::start(&rig).await;
    let before = Rows::read(&rig.state_db());

    let mut new = rig.frontend("new", Some(old.addrs));
    assert_eq!(daemon.ended().await.code(), Code::Unavailable);
    let released = Rows::read(&rig.state_db());
    assert_eq!(released.generation(), before.generation() + 1);
    assert_eq!(
        released.epoch(),
        before.epoch(),
        "the successor kept the epoch"
    );
    assert!(!new.proc.kill().success());

    // The restart: the old process closes its copies of the listeners just after it says
    // `released`, so a first try may find the address in use; anything else is a failure.
    let mut tries = 0;
    let restarted = loop {
        tries += 1;
        let mut proc = rig.spawn_frontend(&format!("restart-{tries}"), Some(old.addrs));
        match proc.start_line() {
            Ok(_) => break proc,
            Err(status) => {
                let log = proc.log();
                assert!(
                    status.code() == Some(2) && log.contains("in use") && tries < 100,
                    "the restart failed (try {tries}, {status}):\n{log}"
                );
            }
        }
    };
    let after = Rows::read(&rig.state_db());
    assert_eq!(
        after.generation(),
        before.generation() + 2,
        "the old process wrote, or the restart did not take the next generation"
    );
    assert_eq!(
        after.epoch(),
        before.epoch() + 1,
        "a cold restart bumps the epoch"
    );
    assert_eq!(after.keys("op/"), before.keys("op/"));

    let mut daemon = Daemon::connect(old.addrs.worker, "node-a").await;
    assert_ne!(daemon.epoch, lease.term);
    daemon.heartbeat(&[]).await;
    let rerun = daemon.start().await.lease_id.expect("a lease id");
    assert_ne!(rerun, lease);
    let reached = Client::connect(old.addrs.reapi).await;
    let result = reached.output("built by the restart").await;
    assert!(daemon.report(rerun, &result).await.accepted);
    let response = done_once(&mut ops).await;
    assert!(answered_with(&response, &result), "{response:?}");
    assert_eq!(
        Rows::read(&rig.state_db()).generation(),
        before.generation() + 2,
        "the old process wrote after it let go"
    );
    drop((ops, client, reached));
    assert!(old.proc.wait_exit().success(), "{}", old.proc.log());
    drop(restarted);
}

/// Catches: a result reported across the switch that is lost, answered twice or answered
/// wrong. The daemon holds a lease when the new frontend takes over; its stream ends with
/// UNAVAILABLE ("connect again"); it dials the new frontend, which kept the epoch, lists the lease
/// in a heartbeat and reports the result there. The client's stream, still on the old
/// process, must be answered with exactly that result, once, and end cleanly; the same
/// report again is not accepted; the state file has one `done/` row and no `op/` row.
/// Mutants: the follower treating an ended operation as no answer (the client's stream
/// hangs), the successor bumping the epoch (the report is refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_result_reported_across_the_switch_is_answered_exactly_once() {
    let rig = Rig::start("result_in_flight");
    let Build {
        old,
        client,
        mut daemon,
        mut ops,
        lease,
        ..
    } = Build::start(&rig).await;

    let new = rig.frontend("new", Some(old.addrs));
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    assert!(ended.message().contains("connect again"), "{ended}");

    let mut daemon = Daemon::connect(new.addrs.worker, "node-a").await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    daemon.heartbeat(&[lease]).await;
    let reached = Client::connect(new.addrs.reapi).await;
    let result = reached.output("reported across the switch").await;
    assert!(daemon.report(lease, &result).await.accepted);
    assert!(
        !daemon.report(lease, &result).await.accepted,
        "a repeated report was accepted"
    );

    let response = done_once(&mut ops).await;
    assert!(answered_with(&response, &result), "{response:?}");
    assert!(!response.cached_result);
    let rows = Rows::read(&rig.state_db());
    assert_eq!(rows.keys("done/").len(), 1, "{:?}", rows.keys(""));
    assert_eq!(rows.keys("op/"), Vec::<String>::new());
    drop((ops, client, reached));
    let mut old = old;
    assert!(old.proc.wait_exit().success(), "{}", old.proc.log());
}

/// Catches: client streams broken by the update, or calls after it that land on the old
/// process. One channel (one HTTP/2 connection to the old process) holds an Execute and
/// a WaitExecution stream of a running operation; the new frontend takes over. The old
/// process sends GOAWAY, so a call made now on the same channel goes to the new process
/// (the proof is that the daemon, which is only on the new one, is handed its action),
/// with at most UNAVAILABLE, the one code Buck2 asks again, on the way. When the daemon
/// reports, both open streams end with the result and no error, and the old process exits
/// 0 once the client's connections are closed. Mutants: the stand down not draining (no
/// GOAWAY: the new call stays on the old process, which refuses it UNAVAILABLE for ever),
/// the follower answering Execute streams and not WaitExecution streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_streams_finish_across_goaway_and_new_calls_land_on_the_new_process() {
    let rig = Rig::start("goaway");
    let Build {
        mut old,
        client,
        mut daemon,
        mut ops,
        name,
        lease,
        ..
    } = Build::start(&rig).await;
    let mut waiting = client.wait(&name).await.expect("WaitExecution");
    let _ = first(&mut waiting).await;

    let new = rig.frontend("new", Some(old.addrs));
    assert_eq!(daemon.ended().await.code(), Code::Unavailable);
    let mut daemon = Daemon::connect(new.addrs.worker, "node-a").await;
    assert_eq!(daemon.epoch, lease.term);
    daemon.heartbeat(&[lease]).await;

    // A new build on the same channel.
    let other = Job::new("a build started after the update");
    client.upload_job(&other).await;
    let (mut fresh, _refused) = execute_with_retry(&client, &other, 20).await;
    let started = daemon.start().await;
    let fresh_lease = started.lease_id.expect("a lease id");
    assert_ne!(fresh_lease, lease);

    let result = client.output("started after the update").await;
    assert!(daemon.report(fresh_lease, &result).await.accepted);
    let response = done_once(&mut fresh).await;
    assert!(answered_with(&response, &result), "{response:?}");

    // The build the update found running ends on both of its streams.
    let built = client.output("built across the update").await;
    assert!(daemon.report(lease, &built).await.accepted);
    let executed = done_once(&mut ops).await;
    let waited = done_once(&mut waiting).await;
    assert!(answered_with(&executed, &built), "{executed:?}");
    assert!(answered_with(&waited, &built), "{waited:?}");

    drop((ops, waiting, fresh, client));
    assert!(old.proc.wait_exit().success(), "{}", old.proc.log());
}

/// Catches: an old instance that systemd restarts after a crash (`Restart=on-failure`) and
/// that flip-flops with the instance now serving, or leaves the store at a wrong generation.
/// A restarted old instance has the same flags as any successor, so it must take over from
/// the serving one by the ordinary handover (the generation the serving one holds, the
/// epoch kept), and the instance it replaces must exit 0 so that systemd does not restart
/// it in turn. The test hands over, lets the old process finish and exit, starts it again, and requires
/// the restart to serve at generation + 2 with the epoch kept, the middle instance to exit
/// 0, and the store untouched by anyone else. (The refusal of a store at another generation
/// than the serving one reported is `a_successor_told_another_generation_exits_2...`:
/// exit 2 is in the unit's `RestartPreventExitStatus`.) Mutants: the handed-over instance
/// exiting non-zero once released (systemd would restart it, a flip-flop); `Released` sent
/// before the store is demoted (the restart cannot lock the file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_old_instance_takes_over_once_and_the_instance_it_replaces_exits_0() {
    let rig = Rig::start("old_restarted");
    let Build {
        mut old,
        client,
        mut daemon,
        ops,
        ..
    } = Build::start(&rig).await;
    let before = Rows::read(&rig.state_db());

    let mut new = rig.frontend("new", Some(old.addrs));
    assert_eq!(daemon.ended().await.code(), Code::Unavailable);
    drop((ops, client));
    assert!(old.proc.wait_exit().success(), "{}", old.proc.log());

    // The old instance, started again by systemd after a failure: same flags.
    let mut again = rig.frontend("old-again", Some(old.addrs));
    assert!(new.proc.wait_exit().success(), "{}", new.proc.log());
    let after = Rows::read(&rig.state_db());
    assert_eq!(after.generation(), before.generation() + 2);
    assert_eq!(after.epoch(), before.epoch(), "a handover keeps the epoch");
    assert_eq!(after.keys("op/"), before.keys("op/"));
    // Nothing flips back: the instance that took over keeps serving.
    assert!(again.proc.start_line().is_ok(), "{}", again.proc.log());
    assert_eq!(
        Rows::read(&rig.state_db()).generation(),
        before.generation() + 2
    );
}
