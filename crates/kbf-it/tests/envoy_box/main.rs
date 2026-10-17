//! The box render of `kbf-deploy envoy render --topology box`, run by the pinned Envoy in
//! front of two stub kbf instances (`stub.rs`), for what `envoy check` can only read:
//! that the settings do what the note's table says they do.
//!
//! - E3: a silent `Execute` or `WaitExecution` outlives the route timeout and the
//!   connection manager's idle timeout (scaled to seconds), and the same stream is cut
//!   once the render's setting is put back to a timeout.
//! - E4: an instance that turns not ready gets no new stream or session, loses no open
//!   one, and no call is ever refused; with `close_connections_on_host_health_failure`
//!   true a daemon's session is cut.
//! - E5: as many silent streams as the ceiling are held open, and are refused through
//!   Envoy's default breakers or a connection cap that is too low.
//! - E7: a real hot restart: an open stream and a worker session outlive the drain time,
//!   both are cut at the parent shutdown time, and the parent is then gone.
//!
//! Ignored in the plain test run: the binary is not in the repository. CI's test-x86 job
//! fetches the pinned release (`cargo xtask fetch`, sha256) and runs these with
//! `--ignored`, naming the binary in `KBF_TEST_ENVOY`; run without it each fails rather
//! than passes. A control in each test plants the defect the setting prevents and must see
//! it happen, so a test cannot pass because Envoy never did what it is asked about.
//! Durations are scaled and nothing asserts how long anything took.

mod harness;
mod stub;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use harness::{Call, End, Farm, HANG, Opts, wait, wait_within};
use serde_json::{Value, json};
use stub::FOR_EVER;

const IGNORE: &str = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY";
const HCM: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config";
const EXECUTE: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes/0/route";
const WAIT_EXECUTION: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes/1/route";
const REAPI: &str = "/static_resources/clusters/0";
const WORKERS: &str = "/static_resources/clusters/1";

fn set(e: &mut Value, pointer: &str, value: Value) {
    *e.pointer_mut(pointer)
        .unwrap_or_else(|| panic!("no {pointer} in the render")) = value;
}

fn remove(e: &mut Value, pointer: &str, key: &str) {
    let removed = e
        .pointer_mut(pointer)
        .and_then(|v| v.as_object_mut())
        .and_then(|o| o.remove(key));
    assert!(removed.is_some(), "no {key} at {pointer}");
}

fn hold(farm: &Farm, ms: u64) {
    farm.a.hold_ms.store(ms, Ordering::SeqCst);
    farm.b.hold_ms.store(ms, Ordering::SeqCst);
}

// --- E3: long silent streams ----------------------------------------------------------

/// An `Execute` and a `WaitExecution` that stay silent for 5 s through a farm whose
/// render `patch` changes: how each ended.
async fn silent_calls(name: &str, patch: impl FnOnce(&mut Value)) -> (End, End) {
    let farm = Farm::start(name, &Opts::default(), patch).await;
    hold(&farm, 5_000);
    let ch = farm.channel().await;
    let execute = Call::open(&ch).await.expect("open Execute");
    let wait_for = Call::wait_for(&ch).await.expect("open WaitExecution");
    (execute.finish().await, wait_for.finish().await)
}

/// Catches a route timeout left at Envoy's default (15 s) or set to a value: the render's
/// timeout of 0 holds a silent stream open to its end, and the control, the same route
/// with a 2 s timeout (the default's scaled stand-in), cuts the same stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn a_silent_stream_outlives_the_route_timeout() {
    let _ = IGNORE;
    let (execute, wait_for) = silent_calls("e3-route", |_| {}).await;
    assert_eq!((execute, wait_for), (End::Done, End::Done));
    let (execute, wait_for) = silent_calls("e3-route-control", |e| {
        set(e, &format!("{EXECUTE}/timeout"), json!("2s"));
        set(e, &format!("{WAIT_EXECUTION}/timeout"), json!("2s"));
    })
    .await;
    assert_ne!(execute, End::Done, "the control Execute was not cut");
    assert_ne!(wait_for, End::Done, "the control WaitExecution was not cut");
}

/// Catches an action that outlives the connection manager's idle timeout being cut: the
/// render's route-level `idle_timeout: 0s` on Execute and WaitExecution holds a stream
/// silent for 5 s through a connection manager idle timeout of 2 s (the 7200 s scaled),
/// and the control, the same render without those two route settings, cuts both.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn a_silent_stream_outlives_the_connection_managers_idle_timeout() {
    let scaled = |e: &mut Value| set(e, &format!("{HCM}/stream_idle_timeout"), json!("2s"));
    let (execute, wait_for) = silent_calls("e3-idle", scaled).await;
    assert_eq!((execute, wait_for), (End::Done, End::Done));
    let (execute, wait_for) = silent_calls("e3-idle-control", |e| {
        scaled(e);
        remove(e, EXECUTE, "idle_timeout");
        remove(e, WAIT_EXECUTION, "idle_timeout");
    })
    .await;
    assert_ne!(execute, End::Done, "the control Execute was not cut");
    assert_ne!(wait_for, End::Done, "the control WaitExecution was not cut");
}

// --- E4: an instance turning not ready ------------------------------------------------

/// What calls through Envoy saw while instance `a` turned not ready.
struct Flip {
    /// Calls that started before Envoy marked `a` failed and reached `a`.
    before_on_a: usize,
    /// Calls that started after and reached `a`.
    after_on_a: usize,
    /// The errors of every call.
    errors: Vec<String>,
    /// How the stream opened on `a` before the flip ended.
    held: End,
}

/// Calls through Envoy while instance `a` turns not ready.
async fn reapi_flip(name: &str, patch: impl FnOnce(&mut Value)) -> Flip {
    let farm = Farm::start(name, &Opts::default(), patch).await;
    hold(&farm, FOR_EVER);
    let ch = farm.channel().await;
    let mut others = Vec::new();
    let mut on_a = None;
    for _ in 0..20 {
        let call = Call::open(&ch).await.expect("open");
        if call.from == "a" {
            on_a = Some(call);
            break;
        }
        others.push(call);
    }
    let on_a = on_a.expect("no stream reached a in 20 opens");
    hold(&farm, 20);
    let after = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(std::sync::Mutex::new(
        Vec::<(bool, Result<String, String>)>::new(),
    ));
    let load = {
        let (ch, after, stop, seen) = (ch.clone(), after.clone(), stop.clone(), seen.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let started_after = after.load(Ordering::SeqCst);
                let result = match Call::open(&ch).await {
                    Ok(call) => {
                        let from = call.from.clone();
                        match call.finish().await {
                            End::Done => Ok(from),
                            other => Err(format!("{other:?}")),
                        }
                    }
                    Err(e) => Err(e),
                };
                seen.lock().expect("lock").push((started_after, result));
            }
        })
    };
    farm.a
        .ready
        .store(false, std::sync::atomic::Ordering::SeqCst);
    farm.wait_unhealthy("kbf_reapi", farm.inst[0].reapi).await;
    after.store(true, Ordering::SeqCst);
    let enough = || async {
        seen.lock()
            .expect("lock")
            .iter()
            .filter(|(a, _)| *a)
            .count()
            >= 20
    };
    wait(enough).await.expect("no calls after the flip");
    stop.store(true, Ordering::SeqCst);
    load.await.expect("load");
    let (errors, before_on_a, after_on_a) = {
        let seen = seen.lock().expect("lock");
        let errors = seen
            .iter()
            .filter_map(|(_, r)| r.as_ref().err().cloned())
            .collect();
        let count_on_a = |late: bool| {
            seen.iter()
                .filter(|(after, r)| *after == late && r.as_ref().is_ok_and(|from| from == "a"))
                .count()
        };
        (errors, count_on_a(false), count_on_a(true))
    };
    farm.a.release();
    Flip {
        before_on_a,
        after_on_a,
        errors,
        held: on_a.finish().await,
    }
}

/// Catches a health failure that cuts the stream an instance holds, refuses a call, or
/// keeps sending new streams to the instance after Envoy marked it failed: the stream
/// opened on `a` before it turned not ready ends cleanly, every call across the flip
/// succeeds (none is a local 503), and none that started after Envoy's check failed
/// reached `a`, though calls did reach it before. (`close_connections_on_host_health_
/// failure` true on this HTTP cluster does not cut the stream on the pinned Envoy, as the
/// flag's documentation, which names `tcp_proxy` only, says; the worker test has the
/// control that shows it cutting.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn an_instance_turning_not_ready_keeps_its_streams_and_gets_no_new_ones() {
    let flip = reapi_flip("e4-reapi", |_| {}).await;
    assert_eq!(flip.errors, Vec::<String>::new());
    // Calls did go to `a` while it was healthy, so none after is not for want of trying.
    assert!(
        flip.before_on_a > 0,
        "no call reached a before its check failed"
    );
    assert_eq!(flip.after_on_a, 0, "calls reached a after its check failed");
    assert_eq!(flip.held, End::Done);
}

/// What a daemon's session sees when instance `a` turns not ready: (whether the session
/// held on `a` was closed within 5 s of Envoy's check failing, the sessions that reached
/// `a` among ten opened after the check failed).
async fn worker_flip(name: &str, patch: impl FnOnce(&mut Value)) -> (bool, u64) {
    let farm = Farm::start(name, &Opts::default(), patch).await;
    let total = || farm.a.sessions.load(Ordering::SeqCst) + farm.b.sessions.load(Ordering::SeqCst);
    let mut sessions = Vec::new();
    while farm.a.sessions.load(Ordering::SeqCst) == 0 {
        assert!(sessions.len() < 20, "no session reached a in 20");
        sessions.push(farm.session().await);
        let n = sessions.len() as u64;
        wait(|| async { total() >= n })
            .await
            .expect("session accepted");
    }
    farm.a.ready.store(false, Ordering::SeqCst);
    farm.wait_unhealthy("kbf_workers", farm.inst[0].worker)
        .await;
    let before = farm.a.sessions.load(Ordering::SeqCst);
    let b_before = farm.b.sessions.load(Ordering::SeqCst);
    for _ in 0..10 {
        sessions.push(farm.session().await);
    }
    let ten = || async { farm.b.sessions.load(Ordering::SeqCst) >= b_before + 10 };
    wait_within(Duration::from_secs(10), ten)
        .await
        .expect("ten new sessions on b");
    let new_on_a = farm.a.sessions.load(Ordering::SeqCst) - before;
    let closed = || async { farm.a.sessions_closed.load(Ordering::SeqCst) >= 1 };
    let cut = wait_within(Duration::from_secs(5), closed).await.is_some();
    (cut, new_on_a)
}

/// Catches `close_connections_on_host_health_failure` true on the worker cluster (which
/// would cut every daemon's quiet session when an instance only starts to drain) and
/// new sessions still reaching an instance Envoy marked failed. The control plants the
/// flag and must see the session closed, so the first half cannot pass because nothing
/// ever closes a session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn a_daemons_session_survives_its_instance_turning_not_ready() {
    let (cut, new_on_a) = worker_flip("e4-worker", |_| {}).await;
    assert!(!cut, "the session on a was closed");
    assert_eq!(new_on_a, 0, "sessions reached a after its check failed");
    let (cut, _) = worker_flip("e4-worker-control", |e| {
        set(
            e,
            &format!("{WORKERS}/close_connections_on_host_health_failure"),
            json!(true),
        )
    })
    .await;
    assert!(cut, "the control did not close the session");
}

// --- E5: as many silent streams as the ceiling ----------------------------------------

const CEILING: u32 = 1500;
const MAX_STREAMS: u32 = 100;
/// How long a stream may take to open before it counts as not held.
const OPEN_LIMIT: Duration = Duration::from_secs(20);

/// Open `CEILING` silent `Execute` streams at once (more than Envoy's default limit of
/// 1024) through a farm whose render `patch` changes: (streams held, calls refused). The
/// stubs allow `MAX_STREAMS` per connection, so Envoy needs 15 upstream connections.
async fn hold_ceiling(name: &str, patch: impl FnOnce(&mut Value)) -> (usize, usize, Vec<String>) {
    let opts = Opts {
        ceiling: CEILING,
        max_streams: MAX_STREAMS,
        hot_restart: None,
    };
    let farm = Farm::start(name, &opts, patch).await;
    hold(&farm, FOR_EVER);
    let mut channels = Vec::new();
    for _ in 0..8 {
        channels.push(farm.channel().await);
    }
    let opens = (0..CEILING as usize).map(|i| {
        let ch = channels[i % channels.len()].clone();
        async move {
            // A stream Envoy queues for a connection it may not open waits for ever.
            tokio::time::timeout(OPEN_LIMIT, Call::open(&ch))
                .await
                .unwrap_or_else(|_| Err("still waiting for a connection".to_owned()))
        }
    });
    let results = futures::future::join_all(opens).await;
    let held = results.iter().filter(|r| r.is_ok()).count();
    let mut why: Vec<String> = results
        .iter()
        .filter_map(|r| r.as_ref().err().cloned())
        .collect();
    why.sort();
    why.dedup();
    (held, results.len() - held, why)
}

/// Catches breakers left at Envoy's defaults (the 1025th silent stream is refused) or a
/// connection cap under what the ceiling needs (the streams past the cap's share are
/// refused): the render holds all 1500, and each control plants one of the two defects
/// and must see calls refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn the_ceiling_of_silent_streams_is_held_open() {
    let (held, refused, why) = hold_ceiling("e5", |_| {}).await;
    assert_eq!((held, refused), (CEILING as usize, 0), "{why:?}");
    let (held, refused, _) = hold_ceiling("e5-default-breakers", |e| {
        remove(e, REAPI, "circuit_breakers")
    })
    .await;
    assert!(
        refused > 0 && held < CEILING as usize,
        "held {held}, refused {refused}"
    );
    let (held, refused, _) = hold_ceiling("e5-few-connections", |e| {
        set(
            e,
            &format!("{REAPI}/circuit_breakers/thresholds/0/max_connections"),
            json!(5),
        )
    })
    .await;
    assert!(
        refused > 0 && held < CEILING as usize,
        "held {held}, refused {refused}"
    );
}

// --- E7: Envoy's hot restart ----------------------------------------------------------

/// Catches Envoy's old epoch cutting an open stream or a worker session at the drain time
/// (60 s in the render; 3 s here) instead of at the parent shutdown time (30 s here, the
/// longest action plus 15 minutes in the render), new streams not reaching the new
/// epoch, and a parent that never goes away. The stream held 20 s ends cleanly after
/// the new epoch is serving and the drain time has passed; the session is cut at the
/// parent shutdown time.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn a_hot_restart_keeps_streams_and_sessions_past_the_drain_time() {
    let opts = Opts {
        hot_restart: Some((30, 3)),
        ..Opts::default()
    };
    let mut farm = Farm::start("e7", &opts, |_| {}).await;
    hold(&farm, 20_000);
    let ch = farm.channel().await;
    let silent = Call::open(&ch).await.expect("open");
    let _session = farm.session().await;
    let sessions =
        || farm.a.sessions.load(Ordering::SeqCst) + farm.b.sessions.load(Ordering::SeqCst);
    wait(|| async { sessions() == 1 })
        .await
        .expect("session accepted");
    hold(&farm, 0);
    let _new_epoch = farm.spawn(1);
    farm.wait_epoch(1).await;
    // A stream opened now is served: by the new epoch, which has the listeners.
    let fresh = farm.channel().await;
    let call = Call::open(&fresh).await.expect("open on the new epoch");
    assert_eq!(call.finish().await, End::Done);
    // Past the drain time, nothing the old epoch holds has been cut.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let cut = farm.a.cut.load(Ordering::SeqCst) + farm.b.cut.load(Ordering::SeqCst);
    let closed = farm.a.sessions_closed.load(Ordering::SeqCst)
        + farm.b.sessions_closed.load(Ordering::SeqCst);
    assert_eq!(
        (cut, closed),
        (0, 0),
        "a stream or session cut before the parent shutdown time"
    );
    assert_eq!(silent.finish().await, End::Done);
    // At the parent shutdown time the session is cut and the old epoch exits.
    let gone = || async {
        farm.a.sessions_closed.load(Ordering::SeqCst)
            + farm.b.sessions_closed.load(Ordering::SeqCst)
            >= 1
    };
    wait(gone).await.expect("the session was never cut");
    let status = tokio::time::timeout(HANG, farm.envoy.wait())
        .await
        .expect("the old epoch never exited")
        .expect("wait");
    assert!(status.success(), "{status}:\n{}", farm.log(0));
    let call = Call::open(&farm.channel().await)
        .await
        .expect("open after the old epoch left");
    assert_eq!(call.finish().await, End::Done);
}

/// The control of the test above: a stream that outlives the parent shutdown time (12 s
/// here) is cut with the old epoch, which is why the render sets the time from the
/// longest action. Catches a test above that passes because Envoy never closes the old
/// epoch's streams.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
async fn a_stream_longer_than_the_parent_shutdown_time_is_cut() {
    let opts = Opts {
        hot_restart: Some((12, 3)),
        ..Opts::default()
    };
    let farm = Farm::start("e7-control", &opts, |_| {}).await;
    hold(&farm, FOR_EVER);
    let silent = Call::open(&farm.channel().await).await.expect("open");
    let _new_epoch = farm.spawn(1);
    farm.wait_epoch(1).await;
    let cut =
        || async { farm.a.cut.load(Ordering::SeqCst) + farm.b.cut.load(Ordering::SeqCst) >= 1 };
    wait(cut)
        .await
        .expect("the stream outlived the parent shutdown time");
    assert_ne!(silent.finish().await, End::Done);
}
