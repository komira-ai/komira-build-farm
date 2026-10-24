//! Sockets bound, retried and stopped.

use std::cell::RefCell;
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;

use futures::{FutureExt as _, StreamExt as _};

use super::*;

fn any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("address")
}

/// Catches: a socket that reports an address other than the one it listens on (port 0
/// must come back as the port the system chose), and an address already in use bound
/// anyway or reported without the address.
#[test]
fn a_socket_reports_where_it_listens_and_refuses_an_address_in_use() {
    let bound = Socket::bind(any(), &Binder::new()).expect("bind");
    assert_ne!(bound.local.port(), 0);
    let taken = Socket::bind(bound.local, &Binder::new())
        .err()
        .expect("in use");
    assert!(
        taken.to_string().contains(&bound.local.to_string()),
        "{taken}"
    );
}

/// Catches: a gRPC listener that does not serve connections, one that serves them with
/// Nagle's algorithm on (small replies would wait for a delayed ACK), and the latch: it
/// must end the stream of connections and close the socket at once, wherever the server
/// is in accepting (here: asleep waiting for a connection that never comes), so that
/// nothing accepts on the address afterwards; closing again changes nothing, and neither
/// does closing a listener nobody waits on.
#[tokio::test]
async fn a_grpc_listener_accepts_until_latched_and_then_closes_its_socket() {
    let socket = Socket::bind(any(), &Binder::new()).expect("bind");
    let addr = socket.local;
    let (mut incoming, latch) = socket.grpc(None).expect("registered");
    assert!(
        incoming.next().now_or_never().is_none(),
        "a connection from nowhere"
    );

    let client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let accepted = incoming
        .next()
        .await
        .expect("a connection")
        .expect("accepted");
    assert!(accepted.remote_addr().is_some());
    drop((client, accepted));

    // A task asleep in accept sees the end when the latch closes, though nothing else wakes it.
    let waiting = tokio::spawn(async move { incoming.next().await.is_none() });
    tokio::task::yield_now().await;
    latch.close();
    assert!(
        waiting.await.expect("the task"),
        "still accepting after the latch"
    );
    assert!(
        TcpStream::connect(addr).is_err(),
        "the address still accepts"
    );
    latch.close();

    let socket = Socket::bind(any(), &Binder::new()).expect("bind");
    let (incoming, latch) = socket.grpc(None).expect("registered");
    latch.close();
    drop(incoming);
}

/// Catches: a listener that keeps its socket when the server drops it while a latch (held
/// by a signal that outlives the server) still exists: the address must be free again.
#[tokio::test]
async fn dropping_a_grpc_listener_closes_its_socket_whoever_holds_the_latch() {
    let socket = Socket::bind(any(), &Binder::new()).expect("bind");
    let addr = socket.local;
    let (incoming, _latch) = socket.grpc(None).expect("registered");
    assert!(TcpStream::connect(addr).is_ok());
    drop(incoming);
    assert!(
        TcpStream::connect(addr).is_err(),
        "the address still accepts"
    );
}

/// Catches: an operator API listener that is not the socket bound: it must answer on the
/// address the socket has.
#[tokio::test]
async fn an_http_listener_is_the_socket_bound() {
    let socket = Socket::bind(any(), &Binder::new()).expect("bind");
    let addr = socket.local;
    let listener = socket.http().expect("registered");
    assert_eq!(listener.local_addr().expect("address"), addr);
    assert!(TcpStream::connect(addr).is_ok());
}

/// Asserts that the twin listeners of the operator API are two handles on the socket bound:
/// both report its address, and it goes on accepting when one is dropped, until the other
/// is too.
#[tokio::test]
async fn the_twin_http_listeners_share_one_socket() {
    let socket = Socket::bind(any(), &Binder::new()).expect("bind");
    let addr = socket.local;
    let (first, second) = socket.http_pair().expect("registered");
    assert_eq!(first.local_addr().expect("address"), addr);
    assert_eq!(second.local_addr().expect("address"), addr);
    drop(first);
    assert!(TcpStream::connect(addr).is_ok(), "closed with one handle");
    drop(second);
    assert!(TcpStream::connect(addr).is_err(), "open with no handle");
}

/// Catches: a socket error that does not say which address it was for.
#[test]
fn a_socket_error_names_its_address() {
    let addr: SocketAddr = "127.0.0.1:9".parse().expect("address");
    let error = bind_error(addr)(io::Error::other("no reactor"));
    assert_eq!(error.to_string(), "bind 127.0.0.1:9: no reactor");
}

fn unavailable() -> io::Error {
    io::Error::from(io::ErrorKind::AddrNotAvailable)
}

fn listener() -> TcpListener {
    TcpListener::bind(any()).expect("bind")
}

/// Catches: no retry of an address that is not there yet (the frontend stays down after a
/// reboot), and waits that do not start at 250 ms and double.
#[test]
fn an_unavailable_address_is_retried_until_it_binds() {
    let mut script = vec![Err(unavailable()), Err(unavailable()), Ok(listener())].into_iter();
    let mut waits = Vec::new();
    let mut attempts = 0;
    let mut left = Duration::from_secs(90);
    let got = retry_unavailable(&mut left, &mut |d| waits.push(d), &mut || {
        attempts += 1;
        script.next().expect("no more attempts")
    });
    assert!(got.is_ok());
    assert_eq!(attempts, 3);
    assert_eq!(
        waits,
        [Duration::from_millis(250), Duration::from_millis(500)]
    );
    assert_eq!(left, Duration::from_millis(89_250));
}

/// Catches: a retry of every error (a port in use or a refused bind would wait out the
/// budget before it is reported).
#[test]
fn any_other_bind_error_is_at_once_fatal() {
    for kind in [io::ErrorKind::AddrInUse, io::ErrorKind::PermissionDenied] {
        let mut attempts = 0;
        let mut slept = 0;
        let mut left = Duration::from_secs(90);
        let got = retry_unavailable(&mut left, &mut |_| slept += 1, &mut || {
            attempts += 1;
            Err(io::Error::from(kind))
        });
        assert_eq!(got.err().map(|e| e.kind()), Some(kind));
        assert_eq!((attempts, slept), (1, 0));
    }
}

/// Catches: no bound on the retry (a start that never ends, killed by systemd), and a
/// bound that is exceeded: the waits add up to the budget exactly, the last one cut.
#[test]
fn the_retry_ends_when_the_waits_reach_the_budget() {
    let mut waits = Vec::new();
    let mut attempts = 0;
    let budget = Duration::from_millis(10_100);
    let mut left = budget;
    let got = retry_unavailable(&mut left, &mut |d| waits.push(d), &mut || {
        attempts += 1;
        assert!(attempts < 100, "unbounded");
        Err(unavailable())
    });
    assert_eq!(
        got.err().map(|e| e.kind()),
        Some(io::ErrorKind::AddrNotAvailable)
    );
    assert_eq!(waits.iter().sum::<Duration>(), budget);
    assert_eq!(left, Duration::ZERO);
    assert_eq!(attempts, waits.len() + 1);
    assert!(waits.iter().all(|w| *w <= Duration::from_secs(2)));
}

/// The address `flaky` never lets bind.
const ABSENT: &str = "192.0.2.10:9";

type Slept = Rc<RefCell<Vec<Duration>>>;

/// A binder whose attempts fail with `AddrNotAvailable` until `fails` attempts on that
/// address were made (never, for port 9), and then give a real listener; its waits are recorded, not slept.
fn flaky(budget: Duration, fails: usize) -> (Binder, Slept, Rc<RefCell<Vec<SocketAddr>>>) {
    let slept = Slept::default();
    let tried = Rc::new(RefCell::new(Vec::new()));
    let (log, seen) = (Rc::clone(&slept), Rc::clone(&tried));
    let binder = Binder::scripted(
        budget,
        Box::new(move |addr| {
            let mut seen = seen.borrow_mut();
            seen.push(addr);
            assert!(seen.len() < 10_000, "the retry has no bound");
            let limit = if addr.port() == 9 { usize::MAX } else { fails };
            if seen.iter().filter(|a| **a == addr).count() <= limit {
                Err(unavailable())
            } else {
                Ok(listener())
            }
        }),
        Box::new(move |wait| log.borrow_mut().push(wait)),
    );
    (binder, slept, tried)
}

/// Catches: a budget that is per bind rather than per start: the frontend binds three
/// listeners one after the other, so three waits of the whole budget would outlast the
/// unit's TimeoutStartSec and systemd would kill the start mid-retry. Two sequential binds
/// of an address that never comes spend one budget: the second makes its one attempt and
/// waits for nothing. Catches: `Binder::bind` does not store what is left.
#[test]
fn two_sequential_binds_of_an_absent_address_share_one_budget() {
    let (binder, slept, tried) = flaky(Duration::from_secs(3), usize::MAX);
    let a: SocketAddr = "192.0.2.10:1".parse().expect("address");
    let b: SocketAddr = "192.0.2.10:2".parse().expect("address");
    assert!(binder.bind(a).is_err());
    let first = slept.borrow().iter().sum::<Duration>();
    assert_eq!(first, Duration::from_secs(3));
    let attempts = tried.borrow().len();
    let error = binder.bind(b).expect_err("no budget left");
    assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    assert_eq!(slept.borrow().iter().sum::<Duration>(), first);
    assert_eq!(tried.borrow().len(), attempts + 1);
}

/// Catches: the second bind of a start waiting its own whole budget after the first
/// bound late: it has only what the first left (3 s less 250 + 500 ms of waits), and
/// waits exactly that for an address that never comes. Catches: `Binder::bind` stores
/// back the budget it started with.
#[test]
fn a_bind_waits_only_for_what_an_earlier_bind_left() {
    let (binder, slept, _) = flaky(Duration::from_secs(3), 2);
    assert!(
        binder
            .bind("192.0.2.10:1".parse().expect("address"))
            .is_ok()
    );
    assert_eq!(slept.borrow().len(), 2);
    let before = slept.borrow().iter().sum::<Duration>();
    assert_eq!(before, Duration::from_millis(750));
    assert!(binder.bind(ABSENT.parse().expect("address")).is_err());
    let after = slept.borrow().iter().sum::<Duration>();
    assert_eq!(after, Duration::from_secs(3));
}

/// Catches: a budget that no longer fits the unit's start timeout. The rendered frontend
/// and storage units have `TimeoutStartSec=120`; the binds of one start spend at most
/// BIND_BUDGET of it. Catches: `BIND_BUDGET` 90 s -> 900 s.
#[test]
fn the_bind_budget_is_below_the_units_start_timeout() {
    const TIMEOUT_START_SEC: Duration = Duration::from_secs(120);
    assert!(BIND_BUDGET < TIMEOUT_START_SEC, "{BIND_BUDGET:?}");
}

/// Catches: `Socket::bind` (the frontend's listeners) not retrying an address that is not
/// there yet, whatever the retry itself does. Catches: it calls `TcpListener::bind` itself.
#[test]
fn a_socket_bind_retries_an_unavailable_address() {
    let (binder, slept, tried) = flaky(BIND_BUDGET, 1);
    let socket = Socket::bind("192.0.2.10:1".parse().expect("address"), &binder).expect("bound");
    assert_ne!(socket.local.port(), 0);
    assert_eq!(tried.borrow().len(), 2);
    assert_eq!(*slept.borrow(), [Duration::from_millis(250)]);
}

/// Catches the same for the gRPC listeners of the other modes (`serve::bind`).
/// Catches: it calls `TcpListener::bind` itself.
#[tokio::test]
async fn a_grpc_bind_retries_an_unavailable_address() {
    let (binder, slept, tried) = flaky(BIND_BUDGET, 1);
    let addr = "192.0.2.10:1".parse().expect("address");
    let (_incoming, local) = crate::serve::bind(addr, &binder).expect("bound");
    assert_ne!(local.port(), 0);
    assert_eq!(tried.borrow().len(), 2);
    assert_eq!(*slept.borrow(), [Duration::from_millis(250)]);
}

/// Catches the same for the operator API listener (`serve::bind_api`).
/// Catches: it calls `TcpListener::bind` itself.
#[tokio::test]
async fn an_api_bind_retries_an_unavailable_address() {
    let (binder, slept, tried) = flaky(BIND_BUDGET, 1);
    let addr = "192.0.2.10:1".parse().expect("address");
    let (_listener, local) = crate::serve::bind_api(addr, &binder).expect("bound");
    assert_ne!(local.port(), 0);
    assert_eq!(tried.borrow().len(), 2);
    assert_eq!(*slept.borrow(), [Duration::from_millis(250)]);
}

/// Catches: the frontend's listeners each given a binder of their own (or a binder that
/// is not passed on). Each address here is absent for three attempts (waits of 250, 500
/// and 1000 ms: 1750 ms) and the budget is 2 s: with a budget per bind all three bind,
/// with one shared the second finds 250 ms left and fails. Catches: `Bound::bind` makes a
/// `Binder::new()` per socket.
#[test]
fn the_frontends_listeners_bind_through_one_shared_budget() {
    let (binder, slept, _) = flaky(Duration::from_secs(2), 3);
    let addr = |port: u16| -> SocketAddr { format!("192.0.2.10:{port}").parse().expect("address") };
    let error = crate::frontend::Bound::bind(addr(1), addr(2), Some(addr(3)), &binder)
        .err()
        .expect("the second listener runs out of budget");
    assert!(error.to_string().contains("192.0.2.10:2"), "{error}");
    assert_eq!(
        slept.borrow().iter().sum::<Duration>(),
        Duration::from_secs(2)
    );
}

/// Catches the production bind path losing the address's own error once it gives up:
/// binding a documentation address (192.0.2.1, on no interface) with a small budget runs
/// the real wait between attempts and then fails with `AddrNotAvailable`, which the
/// frontend reports as it is. (The retry rule itself is pinned by the tests above.) The
/// bind runs on a thread the test gives 60 s: a change that removes the bound fails the
/// test instead of hanging the suite (a liveness guard, not a timing assertion).
/// Catches: `Binder::bind` returns its failure as `ErrorKind::Other`.
#[test]
fn a_real_bind_to_an_absent_address_waits_then_says_it_is_not_available() {
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let absent: SocketAddr = "192.0.2.1:0".parse().expect("an address");
        let binder = Binder::within(Duration::from_millis(300));
        let _ = done.send(binder.bind(absent).err().map(|e| e.kind()));
    });
    let kind = result
        .recv_timeout(Duration::from_secs(60))
        .expect("the bind returned: its budget bounds the wait");
    assert_eq!(kind, Some(io::ErrorKind::AddrNotAvailable));
}

/// Catches: a storage node's `/metrics` listener (`--metrics-listen`) that does not retry an
/// address that is not there yet, as its storage socket does: a node on a VPN address would
/// start with the one and die on the other. Both addresses are absent for one attempt, so
/// both bind after one wait each. Catches: `NodeMetrics::bind` calls `TcpListener::bind`
/// itself, or `Socket::bind` is not given the binder.
#[tokio::test]
async fn a_nodes_metrics_listener_retries_an_unavailable_address() {
    let (binder, slept, tried) = flaky(BIND_BUDGET, 1);
    let at = |port: u16| -> SocketAddr { format!("192.0.2.10:{port}").parse().expect("address") };
    let (socket, metrics) =
        crate::metrics::bind_node_sockets(at(1), Some(at(2)), &binder).expect("both bound");
    assert_ne!(socket.addr().port(), 0);
    assert!(metrics.is_some());
    assert_eq!(*tried.borrow(), [at(1), at(1), at(2), at(2)]);
    assert_eq!(
        *slept.borrow(),
        [Duration::from_millis(250), Duration::from_millis(250)]
    );
}

/// Catches: the node's two listeners each given a budget of their own, so a late interface
/// could keep a node waiting twice the unit's start timeout allows. Each address is absent
/// for three attempts (1750 ms of waits) and the budget is 2 s: with a budget per listener
/// both bind, with one shared the second finds 250 ms left and fails, naming its address.
/// Catches: `bind_node_sockets` gives the metrics listener a `Binder::new()`.
#[tokio::test]
async fn a_nodes_two_listeners_bind_through_one_shared_budget() {
    let (binder, slept, _) = flaky(Duration::from_secs(2), 3);
    let at = |port: u16| -> SocketAddr { format!("192.0.2.10:{port}").parse().expect("address") };
    let error = crate::metrics::bind_node_sockets(at(1), Some(at(2)), &binder)
        .err()
        .expect("the second listener runs out of budget");
    assert!(error.to_string().contains("192.0.2.10:2"), "{error}");
    assert_eq!(
        slept.borrow().iter().sum::<Duration>(),
        Duration::from_secs(2)
    );
}

/// Catches: a node with no `--metrics-listen` binding, or failing on, a second listener.
/// Catches: `bind_node_sockets` binds the metrics address when it is `None` (an unspecified
/// one).
#[tokio::test]
async fn a_node_without_a_metrics_address_binds_only_its_storage_socket() {
    let (binder, _, tried) = flaky(BIND_BUDGET, 0);
    let at: SocketAddr = "192.0.2.10:1".parse().expect("address");
    let (_, metrics) = crate::metrics::bind_node_sockets(at, None, &binder).expect("bound");
    assert!(metrics.is_none());
    assert_eq!(*tried.borrow(), [at]);
}
