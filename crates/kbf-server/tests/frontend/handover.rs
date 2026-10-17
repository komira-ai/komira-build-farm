//! `kbf frontend` taking over from a frontend that serves (`kbf_server::handover`): two
//! frontends in this process on one state file, the first with a build in flight and a
//! daemon connected, a fake daemon playing the farm's node.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use kbf_server::frontend::start_or_take_over;
use kbf_server::{Drain, ServeError};
use tokio::task::JoinHandle;

use support::done_within_quiet;
use tonic::Code;

use super::*;

/// A directory of the test's own: the state file, the control socket.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("kbf-frontend-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The flags of a frontend that hands over, serving on `addresses` (new ones if none).
fn handover_flags(
    node: &Node,
    dir: &Dir,
    addresses: Option<(SocketAddr, SocketAddr, SocketAddr)>,
    more: &[&str],
) -> FrontendFlags {
    let mut argv = vec![
        format!("--storage=http://{}", node.addr),
        format!("--handover-socket={}/control.sock", dir.0.display()),
        "--handover-follow-interval-ms=10".to_owned(),
        "--heartbeat-interval-ms=100".to_owned(),
    ];
    if let Some((reapi, worker, api)) = addresses {
        argv.push(format!("--listen={reapi}"));
        argv.push(format!("--worker-listen={worker}"));
        argv.push(format!("--api-listen={api}"));
    }
    if !more.iter().any(|m| m.starts_with("--state-store=")) {
        argv.push(format!(
            "--state-store=sqlite://{}/state.db",
            dir.0.display()
        ));
    }
    argv.extend(more.iter().map(|m| (*m).to_owned()));
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    flags(&argv).expect("flags")
}

/// A frontend serving in this process.
struct Proc {
    reapi: SocketAddr,
    worker: SocketAddr,
    api: SocketAddr,
    drain: Arc<Drain>,
    /// Dropped to stop the frontend.
    _stop: oneshot::Sender<()>,
    served: JoinHandle<Result<(), ServeError>>,
}

impl Proc {
    fn addresses(&self) -> (SocketAddr, SocketAddr, SocketAddr) {
        (self.reapi, self.worker, self.api)
    }

    fn serving(handle: FrontendHandle, stop: oneshot::Sender<()>) -> Self {
        let (reapi, worker, api) = (handle.reapi, handle.worker, handle.api);
        let drain = handle.drain();
        Self {
            reapi,
            worker,
            api: api.expect("the operator API"),
            drain,
            _stop: stop,
            served: tokio::spawn(handle.serve()),
        }
    }

    /// A start with no predecessor.
    async fn cold(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = start_frontend(flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        Self::serving(handle, stop)
    }

    /// A start that takes over from the frontend serving, or starts alone if none is.
    async fn taking_over(flags: &FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = taking_over(flags, stopped).await.expect("takes over");
        Self::serving(handle, stop)
    }

    /// Waits for this frontend to end, which it does once its last REAPI connection has
    /// closed after a handover.
    async fn ended(self) {
        let served = tokio::time::timeout(Duration::from_secs(20), self.served)
            .await
            .expect("the frontend ends")
            .expect("the task");
        served.expect("served without an error");
    }
}

async fn taking_over(
    flags: &FrontendFlags,
    stopped: oneshot::Receiver<()>,
) -> Result<FrontendHandle, Box<dyn std::error::Error>> {
    start_or_take_over(flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
}

/// Catches: a build failed or hung by a frontend update. The first frontend has a daemon
/// connected and an Execute stream open with its action running. A second frontend takes
/// over: the first ends the daemon's stream (the daemon dials again and reaches the
/// second), keeps the client's stream open, and the second, which kept the epoch, takes
/// the result of the lease the first granted. The client's stream, on the first frontend,
/// is answered with that result; a call made after the handover is the second's, and is
/// answered from the cache the result went to; the first ends once its last connection
/// is gone. Mutants: `Keep` replaced by a bump of the epoch (the lease is not taken), the
/// follower ignoring `done/` rows (the stream is never answered), the stand down not
/// ending the daemon's stream (the daemon never reaches the second frontend).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_in_flight_is_finished_across_a_handover() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, None, &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build across the update", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let lease = start.lease_id.expect("a lease id");

    let new = Proc::taking_over(&handover_flags(&node, &dir, Some(old.addresses()), &[])).await;
    assert_eq!(
        new.addresses(),
        old.addresses(),
        "the successor serves the same addresses"
    );
    assert!(old.drain.is_draining());
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");

    let reached = Client::connect(new.reapi, new.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(daemon.heartbeat(&[lease]).await);
    assert!(
        !done_within_quiet(&mut ops).await,
        "the stream was answered before the result"
    );
    let result = support::output(&reached, "built across the update", 0).await;
    assert!(
        daemon.report(ran(Some(lease), &result)).await.accepted,
        "the successor did not take the lease of the frontend before it"
    );
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    assert!(!response(&last).cached_result);

    let mut again = reached.execute(&job.action).await;
    assert!(response(&done(&mut again).await).cached_result);
    drop((ops, again, client));
    old.ended().await;
    drop(new);
}

/// Catches: a failed attempt to take over that changes the frontend serving. The second
/// frontend is started with a policy file that does not exist: it fails after the first
/// has offered its listeners and before it says it is ready. The first goes on serving
/// (not draining, its daemon's stream open, a new Execute answered), and a second
/// attempt, with good flags, takes over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_fails_before_ready_leaves_the_frontend_serving() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, None, &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;

    let missing = dir.0.join("no-such-policy.json");
    let bad = handover_flags(
        &node,
        &dir,
        Some(old.addresses()),
        &[&format!("--reapi-auth-policy={}", missing.display())],
    );
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&bad, stopped)
        .await
        .err()
        .expect("the start fails");
    assert!(error.to_string().contains("no-such-policy"), "{error}");
    assert!(!old.drain.is_draining(), "the frontend serving was touched");

    let job = Job::new("served by the first", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "still the first", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));

    let new = Proc::taking_over(&handover_flags(&node, &dir, Some(old.addresses()), &[])).await;
    assert_eq!(new.reapi, old.reapi);
    drop((ops, client));
    old.ended().await;
}

/// Catches: a frontend that takes over from one started with other flags (another state
/// file or other addresses): the frontend serving refuses, the start fails with its
/// reason, and the frontend serving is untouched. Also a stop signal that arrives before
/// the handover is agreed ends the start and leaves the other frontend alone, and flags
/// that ask for a handover with no state store are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_that_cannot_take_over_says_why_and_changes_nothing() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, None, &[])).await;

    let elsewhere = dir.0.join("other");
    let other_state = handover_flags(
        &node,
        &dir,
        Some(old.addresses()),
        &[&format!(
            "--state-store=sqlite://{}/state.db",
            elsewhere.display()
        )],
    );
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&other_state, stopped)
        .await
        .err()
        .expect("refused");
    assert!(
        error.to_string().contains("refused the handover"),
        "{error}"
    );
    assert!(error.to_string().contains("--state-store"), "{error}");

    let other_ports = handover_flags(&node, &dir, None, &[]);
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&other_ports, stopped)
        .await
        .err()
        .expect("refused");
    assert!(error.to_string().contains("listen flags"), "{error}");

    let same = handover_flags(&node, &dir, Some(old.addresses()), &[]);
    let error = start_or_take_over(&same, || Ok(std::future::ready(())))
        .await
        .err()
        .expect("stopped");
    assert!(
        error.to_string().contains("stopped before the handover"),
        "{error}"
    );
    assert!(!old.drain.is_draining());

    let mut no_state = same.clone();
    no_state.server.state_store = None;
    let (_stop, stopped) = oneshot::channel::<()>();
    let error = taking_over(&no_state, stopped)
        .await
        .err()
        .expect("no state store");
    assert!(error.to_string().contains("needs --state-store"), "{error}");
}

/// Catches: a successor that dies after the frontend serving has let go (the window
/// the handover cannot undo) leaving the farm without a frontend that works, or the
/// callers it kept without an answer: the next start finds no frontend serving (the
/// socket file is stale), binds the same addresses, bumps the epoch and runs the
/// operation again, and the first frontend, which kept the client's stream open,
/// answers it from the result the new one writes. Mutants: the holder keeping its
/// listeners after it released (the addresses are in use), the control socket left
/// accepting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_dies_after_the_release_is_started_again_and_the_streams_finish() {
    let node = Node::start().await;
    let dir = Dir::new();
    let old = Proc::cold(&handover_flags(&node, &dir, None, &[])).await;
    let client = Client::connect(old.reapi, old.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build whose successor dies", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let first = daemon.start().await;

    // It has taken over and dies before it serves.
    let flags = handover_flags(&node, &dir, Some(old.addresses()), &[]);
    let (_stop, stopped) = oneshot::channel::<()>();
    let dying = taking_over(&flags, stopped).await.expect("takes over");
    drop(dying);
    assert!(daemon.ended().await.code() == Code::Unavailable);

    // The frontend that let go closes its handles on the listeners a moment after it says
    // so, and a service manager restarts a failed unit after a delay; the start is tried
    // again for the first.
    let new = loop {
        let (stop, stopped) = oneshot::channel::<()>();
        match taking_over(&flags, stopped).await {
            Ok(handle) => break Proc::serving(handle, stop),
            Err(error) if error.to_string().contains("in use") => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("the start after the crash failed: {error}"),
        }
    };
    assert_eq!(new.reapi, old.reapi);
    let reached = Client::connect(new.reapi, new.worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_ne!(
        daemon.epoch,
        first.lease_id.expect("a lease id").term,
        "a cold start bumps the epoch"
    );
    // A daemon that sees a new epoch kills the runs it had and lists none.
    assert!(daemon.heartbeat(&[]).await);
    let second = daemon.start().await;
    assert_ne!(second.lease_id, first.lease_id, "the operation runs again");
    let result = support::output(&reached, "run again", 0).await;
    assert!(daemon.report(ran(second.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
    drop((ops, client));
    old.ended().await;
}

/// Catches: `READY=1` sent before the frontend serves, never sent after, or sent by a
/// frontend that cannot reach its storage node: the service manager's socket gets it
/// only once `serve` runs and the probes pass, for a start with no predecessor and for
/// one that took over alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_manager_is_told_when_the_frontend_serves() {
    let node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let expect_ready = |manager: &UnixDatagram| {
        manager
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let mut buffer = [0_u8; 16];
        let got = manager.recv(&mut buffer).expect("READY=1");
        assert_eq!(&buffer[..got], b"READY=1");
    };

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&handover_flags(&node, &dir, None, &[]), stopped)
        .await
        .expect("a start with no predecessor");
    let addresses = (handle.reapi, handle.worker, handle.api.expect("api"));
    let handle = handle.notifying(Some(manager_path.clone().into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    let mut buffer = [0_u8; 16];
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let old = Proc::serving(handle, stop);
    expect_ready(&manager);

    let (stop, stopped) = oneshot::channel::<()>();
    let flags = handover_flags(&node, &dir, Some(addresses), &[]);
    let handle = taking_over(&flags, stopped).await.expect("takes over");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let new = Proc::serving(handle, stop);
    expect_ready(&manager);
    old.ended().await;
    drop(new);
}

/// Catches: flags for a handover that parse without what they need or with what they do
/// not: a socket needs a state store, the two timings need the socket, the defaults are
/// the documented ones; and a start without the flag is a start as `start_frontend` makes
/// it (`start_or_take_over` with nothing to take over from).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handover_flags_need_each_other_and_a_start_without_them_is_a_plain_start() {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    for lone in [
        "--handover-socket=/run/kbf/control.sock",
        "--handover-quiesce-secs=5",
        "--handover-follow-interval-ms=5",
    ] {
        let error = flags(&[&url, lone]).expect_err(lone);
        assert!(error.to_string().contains("--"), "{lone}: {error}");
    }
    assert!(
        flags(&[
            &url,
            "--handover-socket=/s",
            "--state-store=sqlite:///x.db",
            "--handover-follow-interval-ms=0"
        ])
        .is_err()
    );
    let given =
        flags(&[&url, "--handover-socket=/s", "--state-store=sqlite:///x.db"]).expect("flags");
    let handover = given.handover.expect("a handover");
    assert_eq!(handover.socket, PathBuf::from("/s"));
    assert_eq!(handover.quiesce, Duration::from_secs(10));
    assert_eq!(handover.follow_interval, Duration::from_millis(200));
    let tuned = flags(&[
        &url,
        "--handover-socket=/s",
        "--state-store=sqlite:///x.db",
        "--handover-quiesce-secs=3",
        "--handover-follow-interval-ms=50",
    ])
    .expect("flags");
    let handover = tuned.handover.expect("a handover");
    assert_eq!(
        (handover.quiesce, handover.follow_interval),
        (Duration::from_secs(3), Duration::from_millis(50))
    );
    assert!(flags(&[&url]).expect("flags").handover.is_none());

    let (stop, stopped) = oneshot::channel::<()>();
    let plain = taking_over(&flags(&[&url]).expect("flags"), stopped)
        .await
        .expect("a plain start");
    let served = tokio::spawn(plain.serve());
    drop(stop);
    served.await.expect("the task").expect("served");
}

/// Catches: a frontend that starts without its control socket (it would be unable to hand
/// over, and the operator would find out at the update): a socket that cannot be made
/// (here, in a directory that does not exist) fails the start, with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_socket_that_cannot_be_made_fails_the_start() {
    let node = Node::start().await;
    let dir = Dir::new();
    let nowhere = dir.0.join("missing").join("control.sock");
    let state = format!("--state-store=sqlite://{}/state.db", dir.0.display());
    let socket = format!("--handover-socket={}", nowhere.display());
    let url = format!("--storage=http://{}", node.addr);
    let flags = flags(&[&url, &state, &socket]).expect("flags");
    let error = start_frontend(&flags, || Ok(std::future::pending::<()>()))
        .err()
        .expect("no control socket");
    assert!(error.to_string().contains("No such file"), "{error}");
}
