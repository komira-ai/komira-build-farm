//! `Vm`, `Ctx` and `spawn_retrying`, against scripted helpers.

use std::time::Duration;

use crate::bundle::Identity;
use crate::fake::{FakeSystem, Script, ctx, work_with_base};
use crate::probe::{Boot, Timing, Vm};
use crate::spec::{Event, LIMIT_EXCEEDED, VmSpec};
use crate::system::Poll;

fn vm(sys: &FakeSystem, work: &std::path::Path) -> Vm {
    Vm::spawn(sys, "v", &VmSpec::minimal(work.join("base")), None).expect("spawn")
}

/// Catches a boot verdict that does not require the start event, the hold, or both.
#[test]
fn a_started_vm_that_stays_up_has_booted() {
    let work = work_with_base("probe-boot");
    let sys = FakeSystem::new();
    let mut v = vm(&sys, &work);
    let b = v.await_boot(&Timing::instant());
    assert_eq!(
        b,
        Boot {
            booted: true,
            started_ms: Some(100),
            failure: None,
            code: None
        }
    );
    assert_eq!(b.json()["booted"], true);
}

/// Catches each way a start goes wrong being read as a boot: no start event, an error
/// event with its code, a stop after the start, the helper ending, a state change to
/// error or stopped.
#[test]
fn every_failure_is_not_a_boot() {
    let work = work_with_base("probe-fail");
    let t = Timing::instant();
    let cases: Vec<(Script, &str)> = vec![
        (
            Script {
                events: vec![],
                guest_files: vec![],
                exits_on_stop: true,
                log: None,
            },
            "no started event",
        ),
        (
            Script::failing("bad disk", Some("InvalidDiskImage")),
            "bad disk",
        ),
        (
            Script::booted().then(Poll::Event(Event::Stopped { ms: 5 })),
            "stopped",
        ),
        (
            Script::booted().then(Poll::Gone {
                code: Some(1),
                signal: None,
            }),
            "helper ended",
        ),
        (
            Script::booted().then(Poll::Event(Event::State {
                state: "error".into(),
                ms: 5,
            })),
            "state error",
        ),
        (
            Script::booted().then(Poll::Event(Event::State {
                state: "stopped".into(),
                ms: 5,
            })),
            "state stopped",
        ),
        (
            Script {
                events: vec![Poll::Gone {
                    code: None,
                    signal: Some(9),
                }],
                guest_files: vec![],
                exits_on_stop: true,
                log: None,
            },
            "signal Some(9)",
        ),
    ];
    for (script, want) in cases {
        let sys = FakeSystem::new();
        sys.script(script);
        let b = vm(&sys, &work).await_boot(&t);
        assert!(!b.booted, "{want}: {b:?}");
        assert!(
            b.failure
                .expect("failure")
                .contains(want.split(' ').next_back().expect("word")),
            "{want}"
        );
    }
    let sys = FakeSystem::new();
    sys.script(Script::failing("limit", Some(LIMIT_EXCEEDED)));
    let b = vm(&sys, &work).await_boot(&t);
    assert_eq!(b.code.as_deref(), Some(LIMIT_EXCEEDED));
    // A state that is not terminal does not fail the boot.
    sys.script(Script::booted().then(Poll::Event(Event::State {
        state: "running".into(),
        ms: 5,
    })));
    assert!(vm(&sys, &work).await_boot(&t).booted);
}

/// Catches a file wait that gives up before the file appears, or never gives up.
#[test]
fn waiting_for_a_file() {
    let work = work_with_base("probe-file");
    let sys = FakeSystem::new();
    let mut v = vm(&sys, &work);
    let t = Duration::from_millis(1);
    assert!(
        v.wait_file(&work.join("base/disk.img"), Duration::from_millis(50), t)
            .is_some()
    );
    assert!(
        v.wait_file(&work.join("never"), Duration::from_millis(5), t)
            .is_none()
    );
    // A helper that has ended will not write it: do not wait out the time.
    sys.script(Script {
        events: vec![Poll::Gone {
            code: Some(0),
            signal: None,
        }],
        guest_files: vec![],
        exits_on_stop: true,
        log: None,
    });
    let mut gone = vm(&sys, &work);
    let began = std::time::Instant::now();
    assert!(
        gone.wait_file(&work.join("never"), Duration::from_secs(30), t)
            .is_none()
    );
    assert!(began.elapsed() < Duration::from_secs(5));
}

/// Catches a stop that reports success for a helper that had to be killed, and a kill
/// that does not leave the helper gone.
#[test]
fn stop_asks_first_and_kills_when_ignored() {
    let work = work_with_base("probe-stop");
    let t = Timing::instant();
    let sys = FakeSystem::new();
    let mut polite = vm(&sys, &work);
    assert!(polite.stop(&t));
    sys.script(Script {
        exits_on_stop: false,
        log: None,
        ..Script::booted()
    });
    let mut stubborn = vm(&sys, &work);
    assert!(!stubborn.stop(&t));
    assert!(matches!(
        stubborn.proc.poll(t.tick),
        Poll::Gone {
            signal: Some(9),
            ..
        }
    ));
    let mut victim = vm(&sys, &work);
    victim.kill9(&t);
    assert!(matches!(
        victim.proc.poll(t.tick),
        Poll::Gone {
            signal: Some(9),
            ..
        }
    ));
}

/// Catches traffic that is not read from the events: connections and console text.
#[test]
fn guest_traffic_lists_connections_and_console_writes() {
    let work = work_with_base("probe-traffic");
    let sys = FakeSystem::new();
    sys.script(
        Script::booted()
            .then(Poll::Event(Event::Vsock { port: 5000 }))
            .then(Poll::Event(Event::Console {
                port: "kbf.probe".into(),
                text: "hi".into(),
            })),
    );
    let mut v = vm(&sys, &work);
    v.await_boot(&Timing::instant());
    let t = v.guest_traffic();
    assert_eq!(t["vsock_connections"], serde_json::json!([5000]));
    assert_eq!(t["console_writes"][0]["text"], "hi");
}

/// Catches a retry that stops at the first limit error, one that retries other errors,
/// and one that never gives up.
#[test]
fn a_start_is_retried_only_for_the_guest_limit() {
    let work = work_with_base("probe-retry");
    let sys = FakeSystem::new();
    let c = ctx(&sys, &work);
    let spec = VmSpec::minimal(work.join("base"));
    assert_eq!(c.spawn_retrying("a", &spec).expect("first").attempts, 1);
    sys.script(Script::limit());
    let r = c.spawn_retrying("b", &spec).expect("second");
    assert_eq!(r.attempts, 2);
    assert!(r.waited >= Duration::ZERO);
    sys.script(Script::failing("bad disk", None));
    assert_eq!(
        c.spawn_retrying("c", &spec).err().as_deref(),
        Some("bad disk")
    );
    sys.script(Script {
        events: vec![],
        guest_files: vec![],
        exits_on_stop: true,
        log: None,
    });
    assert!(
        c.spawn_retrying("d", &spec)
            .err()
            .expect("timeout")
            .contains("no started event")
    );
    for _ in 0..200 {
        sys.script(Script::limit());
    }
    assert!(
        c.spawn_retrying("e", &spec)
            .err()
            .expect("window")
            .contains("still at the guest limit")
    );
    *sys.start_error.borrow_mut() = Some("cannot spawn".into());
    assert_eq!(
        c.spawn_retrying("f", &spec).err().as_deref(),
        Some("cannot spawn")
    );
}

/// Catches a clone started with the spec edit lost, a user id lost, or a clone error
/// swallowed.
#[test]
fn a_clone_is_started_with_the_edited_spec() {
    let work = work_with_base("probe-clone");
    let sys = FakeSystem::new();
    let c = ctx(&sys, &work);
    let (_vm, took) = c
        .start_clone("one", Identity::Keep, |s| s.cpus = 7, Some(77))
        .expect("clone");
    assert!(took >= Duration::ZERO);
    let starts = sys.starts.borrow();
    assert_eq!((starts[0].0.cpus, starts[0].1), (7, Some(77)));
    assert_eq!(starts[0].0.bundle, work.join("one"));
    drop(starts);
    *sys.clone_fails_on.borrow_mut() = Some("disk.img");
    assert!(
        c.start_clone("two", Identity::Keep, |_| {}, None)
            .err()
            .expect("err")
            .contains("EXDEV")
    );
}

/// Catches timings that do not differ between real and test use (a test that waits as
/// long as a real run).
#[test]
fn the_real_timing_is_longer_than_the_test_timing() {
    assert!(Timing::real().hold > Timing::instant().hold);
    assert!(Timing::real().guest_timeout > Duration::from_secs(60));
}

/// Catches a helper that dies without the cause reaching the report: the tail of its
/// log is part of the failure, and an empty log adds nothing.
#[test]
fn a_helper_that_ended_reports_its_log_tail() {
    let work = work_with_base("probe-log-tail");
    let sys = FakeSystem::new();
    let gone = || Poll::Gone {
        code: Some(1),
        signal: None,
    };
    sys.script(
        Script::booted()
            .then(gone())
            .with_log("  cannot execute: permission denied\n"),
    );
    sys.script(Script::booted().then(gone()).with_log("   \n"));
    let t = Timing::instant();
    let with = vm(&sys, &work).await_boot(&t).failure.expect("failure");
    assert!(
        with.contains("helper log: cannot execute: permission denied"),
        "{with}"
    );
    let blank = vm(&sys, &work).await_boot(&t).failure.expect("failure");
    assert!(!blank.contains("helper log"), "{blank}");
}

/// Catches a start time read from the wrong event, and a clone onto an existing
/// directory deleting what was there.
#[test]
fn the_start_time_is_the_started_event_and_an_existing_clone_is_kept() {
    let work = work_with_base("probe-started");
    let sys = FakeSystem::new();
    sys.script(Script {
        events: vec![
            Poll::Event(Event::Configured { ms: 1 }),
            Poll::Event(Event::Started { ms: 42 }),
        ],
        guest_files: vec![],
        exits_on_stop: true,
        log: None,
    });
    let mut v = vm(&sys, &work);
    assert!(v.await_boot(&Timing::instant()).booted);
    assert_eq!(v.started_ms(), Some(42));
    let c = ctx(&sys, &work);
    std::fs::create_dir(work.join("taken")).expect("dir");
    std::fs::write(work.join("taken/keep"), "x").expect("file");
    assert!(
        c.start_clone("taken", Identity::Keep, |_| {}, None)
            .is_err()
    );
    assert!(work.join("taken/keep").exists());
}

/// Catches a file wait that ignores a stop or error event and waits out the whole time
/// (the 320 s of a guest that powered off at 30 s), and one that forgets why it ended.
#[test]
fn a_stop_event_ends_the_file_wait_and_says_why() {
    let work = work_with_base("probe-file-stop");
    let sys = FakeSystem::new();
    let t = Duration::from_millis(1);
    for (script, why) in [
        (
            Script::booted().then(Poll::Event(Event::Stopped { ms: 9 })),
            "the VM stopped",
        ),
        (
            Script::booted().then(Poll::Event(Event::State {
                state: "error".into(),
                ms: 9,
            })),
            "the VM went to state error",
        ),
        (
            Script::booted().then(Poll::Event(Event::Error {
                stage: "run".into(),
                message: "guest panicked".into(),
                code: None,
            })),
            "guest panicked",
        ),
    ] {
        sys.script(script);
        let mut v = vm(&sys, &work);
        let began = std::time::Instant::now();
        assert!(
            v.wait_file(&work.join("never"), Duration::from_secs(60), t)
                .is_none()
        );
        assert!(began.elapsed() < Duration::from_secs(10), "{why}");
        assert_eq!(v.ended.as_deref(), Some(why));
        // A second wait on the same VM does not start the time over.
        let again = std::time::Instant::now();
        assert!(
            v.wait_file(&work.join("never"), Duration::from_secs(60), t)
                .is_none()
        );
        assert!(again.elapsed() < Duration::from_secs(10));
    }
    // A file written before the stop is still found.
    sys.script(Script::booted().then(Poll::Event(Event::Stopped { ms: 9 })));
    let mut v = vm(&sys, &work);
    v.watch(Duration::from_millis(20), t, false);
    assert!(v.ended.is_some());
    assert!(
        v.wait_file(&work.join("base/disk.img"), Duration::from_secs(60), t)
            .is_some()
    );
}

/// Catches a trace that loses an event, pairs it with the wrong time, or reports a run
/// that never ended as ended.
#[test]
fn the_trace_lists_every_event_with_its_arrival_time() {
    let work = work_with_base("probe-trace");
    let sys = FakeSystem::new();
    sys.script(
        Script::booted()
            .then(Poll::Event(Event::State {
                state: "running".into(),
                ms: 5,
            }))
            .then(Poll::Event(Event::Vsock { port: 5000 })),
    );
    let mut v = vm(&sys, &work);
    v.watch(Duration::from_millis(20), Duration::from_millis(1), false);
    let trace = v.trace_json();
    let events = trace["events"].as_array().expect("events");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["event"]["event"], "started");
    assert_eq!(events[1]["event"]["state"], "running");
    assert_eq!(events[2]["event"]["port"], 5000);
    assert!(events.iter().all(|e| e["host_ms"].is_u64()));
    assert_eq!(v.event_ms.len(), v.events.len());
    assert!(trace["ended"].is_null());
}
