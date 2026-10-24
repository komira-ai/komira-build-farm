//! The roll state machine over the fakes: order, stop on first failure, 409, idempotency,
//! abort and the bounds. Restart and corruption are in `updater_restart_tests`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::api::{ErrorCode, Hosts, Phase, Roll, RollRequest, RollState};
use crate::fakes::{
    Event, FakeClock, FakeDrain, FakeHealth, FakeInstaller, FakeSafeToStop, Log, safe,
    unsafe_because,
};
use crate::inventory::Inventory;
use crate::ports::{Health, PortError, Stop};
use crate::record::{Fsync, RollStore};
use crate::{Ports, Timing, Updater};

pub(crate) const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample";

/// st1 is listed before the frontend on purpose: the plan, not the file, puts it first.
pub(crate) fn inventory() -> Inventory {
    Inventory::parse(&format!(
        "st1 st1.farm.example storage {KEY}\n\
         fe  fe.farm.example frontend {KEY}\n\
         dm  dm.farm.example daemon {KEY} dm-node\n\
         st2 st2.farm.example storage {KEY}\n"
    ))
    .expect("inventory")
}

/// The order a roll over everything touches the hosts in.
pub(crate) const ORDER: [&str; 4] = ["fe", "st1", "dm", "st2"];

pub(crate) fn request(version: &str, hosts: Hosts, key: &str) -> RollRequest {
    let body = serde_json::json!({
        "package": "kbf", "version": version, "hosts": hosts, "idempotency_key": key,
    });
    RollRequest::from_json(body.to_string().as_bytes()).expect("request")
}

pub(crate) fn all(version: &str, key: &str) -> RollRequest {
    request(version, Hosts::default(), key)
}

/// A scratch directory and one set of fakes; any number of updaters can be opened over them,
/// as a process that restarts over the same disk and the same hosts.
pub(crate) struct Rig {
    pub dir: PathBuf,
    pub log: Log,
    pub stop: FakeSafeToStop,
    pub inst: FakeInstaller,
    pub drain: FakeDrain,
    pub health: FakeHealth,
    pub clock: FakeClock,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/updater-scratch")
            .join(format!("{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        for leftover in ["roll.json", ".roll.json.tmp"] {
            let _ = fs::remove_file(dir.join(leftover));
        }
        let log = Log::default();
        Rig {
            dir,
            stop: FakeSafeToStop::new(&log),
            inst: FakeInstaller::new(&log),
            drain: FakeDrain::new(&log),
            health: FakeHealth::new(&log),
            clock: FakeClock::new(),
            log,
        }
    }

    pub fn ports(&self) -> Ports {
        Ports {
            safe: Box::new(self.stop.clone()),
            installer: Box::new(self.inst.clone()),
            drain: Box::new(self.drain.clone()),
            health: Box::new(self.health.clone()),
            clock: Box::new(self.clock.clone()),
        }
    }

    pub fn store(&self) -> RollStore {
        RollStore::with_sync(&self.dir, Box::new(Fsync))
    }

    pub fn open(&self) -> Updater {
        self.open_with(self.store())
    }

    pub fn open_with(&self, store: RollStore) -> Updater {
        self.open_inventory(inventory(), store)
    }

    /// An updater over `inventory` in place of the standard one.
    pub fn open_inventory(&self, inventory: Inventory, store: RollStore) -> Updater {
        match Updater::open(inventory, store, self.ports(), Timing::default()) {
            Ok(u) => u,
            Err(e) => panic!("open: {e}"),
        }
    }

    pub fn elapsed(&self) -> Duration {
        crate::ports::Clock::now(&self.clock)
    }

    /// Start and drive a roll over everything to version 2.0.
    pub fn roll(&self, u: &Updater) -> Roll {
        let started = u.start(&all("2.0", "run-1")).expect("start");
        u.drive(&started.roll.id).expect("drive")
    }
}

fn phases(roll: &Roll) -> Vec<(String, Phase)> {
    roll.hosts
        .iter()
        .map(|h| (h.alias.clone(), h.phase))
        .collect()
}

fn failure_code(roll: &Roll) -> ErrorCode {
    roll.failure.as_ref().expect("a failure").code
}

fn e(s: &str) -> Event {
    // `ck:` check, `in:` install, `hl:` health, `qy:` query, `dr:` drain (600 s), `ls:` read the
    // node's leases, `ud:` undrain.
    let (kind, alias) = s.split_once(':').expect("kind:alias");
    let a = alias.to_owned();
    match kind {
        "ck" => Event::Check(a),
        "in" => Event::Install(a, "2.0".to_owned()),
        "hl" => Event::Health(a),
        "dr" => Event::Drain(a, 600),
        "ls" => Event::Leases(a),
        "ud" => Event::Undrain(a),
        _ => Event::Query(a),
    }
}

fn events(list: &[&str]) -> Vec<Event> {
    list.iter().map(|s| e(s)).collect()
}

/// Catches an order that is not "the frontend first, then one host at a time, each fully
/// healthy before the next is asked about": the exact call sequence of a clean roll. The
/// frontend has no safe-to-stop; every node has one, immediately before its install, which
/// is also the re-check after the previous host. A daemon host's node id is asked first, and
/// its node is drained (600 s) and its leases read before the install, then undrained once the
/// host is healthy (a held check, then a check for serving).
#[test]
fn a_clean_roll_goes_frontend_first_then_one_host_at_a_time() {
    let rig = Rig::new("clean");
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(roll.failure, None);
    assert_eq!(roll.id, "roll-1");
    assert!(
        roll.hosts
            .iter()
            .all(|h| h.phase == Phase::Done && h.attempts == 1)
    );
    assert_eq!(
        rig.log.events(),
        events(&[
            "in:fe", "hl:fe", "ck:st1", "in:st1", "hl:st1", "qy:dm", "ck:dm", "dr:dm", "ls:dm",
            "in:dm", "hl:dm", "ud:dm", "hl:dm", "ck:st2", "in:st2", "hl:st2",
        ])
    );
    assert_eq!(
        rig.elapsed(),
        Duration::ZERO,
        "a healthy roll waits for nothing"
    );
    // The same answer from the record and from GET.
    assert_eq!(u.get("roll-1").expect("get"), roll);
    assert_eq!(
        rig.store().load().expect("load").expect("record").roll,
        roll
    );
}

/// Catches a plan that follows the request's order for the frontend (it moves up) or
/// ignores the caller's order for nodes; and a list roll touching hosts it was not given.
#[test]
fn a_listed_roll_touches_only_the_listed_hosts_frontend_first() {
    let rig = Rig::new("listed");
    let u = rig.open();
    let hosts = Hosts::Aliases(vec!["st2".into(), "fe".into(), "dm".into()]);
    let started = u.start(&request("2.0", hosts, "k")).expect("start");
    assert_eq!(
        phases(&started.roll)
            .iter()
            .map(|(a, _)| a.as_str())
            .collect::<Vec<_>>(),
        ["fe", "st2", "dm"]
    );
    let roll = u.drive(&started.roll.id).expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.installs(), ["fe", "st2", "dm"]);
    assert!(rig.log.touching("st1").is_empty());
}

/// Catches installing while the host is not yet stoppable, and the check not being repeated:
/// unsafe answers hold the roll at `checking` (polling, not installing) until safe.
#[test]
fn an_unsafe_host_waits_and_is_installed_only_once_it_is_safe() {
    let rig = Rig::new("unsafe-then-safe");
    rig.stop.script(
        "st1",
        vec![
            Ok(unsafe_because("m2 is behind")),
            Err(PortError("503".into())),
            Ok(unsafe_because("x")),
        ],
        Ok(safe()),
    );
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Done);
    let ev = rig.log.touching("st1");
    assert_eq!(
        ev,
        events(&["ck:st1", "ck:st1", "ck:st1", "ck:st1", "in:st1", "hl:st1"])
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(15));
}

/// Catches a roll that goes on past an unsafe host, or gives up early/late: safe-to-stop
/// unsafe for the whole bound fails the roll at that host with `unsafe_to_stop` and the
/// reason, and nothing after it is asked about.
#[test]
fn unsafe_past_the_bound_fails_the_roll_and_stops_there() {
    let rig = Rig::new("unsafe-bound");
    rig.stop.script(
        "st1",
        vec![],
        Ok(unsafe_because("under-replicated blobs: 3")),
    );
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(roll.state, RollState::Failed);
    assert_eq!(failure_code(&roll), ErrorCode::UnsafeToStop);
    let f = roll.failure.clone().expect("failure");
    assert_eq!(f.alias, "st1");
    assert!(f.detail.contains("under-replicated blobs: 3"));
    assert_eq!(roll.hosts[1].detail.as_deref(), Some(f.detail.as_str()));
    assert_eq!(
        phases(&roll),
        [
            ("fe".into(), Phase::Done),
            ("st1".into(), Phase::Failed),
            ("dm".into(), Phase::Pending),
            ("st2".into(), Phase::Pending)
        ]
    );
    assert_eq!(rig.elapsed(), Timing::default().safe_bound);
    assert!(!rig.log.installs().contains(&"st1".to_owned()));
    for later in ["dm", "st2"] {
        assert!(rig.log.touching(later).is_empty(), "{later} was touched");
    }
}

/// Catches an unreachable safe-to-stop treated as safe (fail open), and mislabelled as
/// unsafe: it is its own code.
#[test]
fn safe_to_stop_unreachable_for_the_bound_fails_closed() {
    let rig = Rig::new("stop-down");
    rig.stop
        .script("st1", vec![], Err(PortError("connection refused".into())));
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(failure_code(&roll), ErrorCode::SafeToStopUnavailable);
    assert!(
        roll.failure
            .expect("f")
            .detail
            .contains("connection refused")
    );
    assert!(!rig.log.installs().contains(&"st1".to_owned()));
}

/// Catches a 404 waited out like an unsafe answer, or let through: an unknown member refuses
/// at once (no sleeping) and nothing is installed on it.
#[test]
fn an_unknown_member_refuses_at_once() {
    let rig = Rig::new("unknown-member");
    rig.stop.script("st1", vec![], Ok(Stop::Unknown));
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(failure_code(&roll), ErrorCode::UnknownMember);
    assert_eq!(rig.elapsed(), Duration::ZERO);
    assert_eq!(rig.log.touching("st1"), events(&["ck:st1"]));
}

/// Catches the first failure not stopping the roll, wherever it falls: for every position in
/// the plan, the hosts before it are done, it is failed with `install_failed`, the hosts after
/// it are pending, and not one call names them.
#[test]
fn the_first_failure_stops_the_roll_and_no_later_host_is_touched() {
    for (at, failing) in ORDER.iter().enumerate() {
        let rig = Rig::new(&format!("stop-at-{failing}"));
        rig.inst
            .script_install(failing, vec![], Err(PortError("dpkg: lock held".into())));
        let u = rig.open();
        let roll = rig.roll(&u);
        assert_eq!(roll.state, RollState::Failed, "{failing}");
        assert_eq!(failure_code(&roll), ErrorCode::InstallFailed);
        for (i, (alias, phase)) in phases(&roll).into_iter().enumerate() {
            let want = match i.cmp(&at) {
                std::cmp::Ordering::Less => Phase::Done,
                std::cmp::Ordering::Equal => Phase::Failed,
                std::cmp::Ordering::Greater => Phase::Pending,
            };
            assert_eq!(phase, want, "{alias} when {failing} fails");
            if i > at {
                assert!(
                    rig.log.touching(&alias).is_empty(),
                    "{alias} touched after {failing}"
                );
            }
        }
        assert_eq!(roll.hosts[at].attempts, 1);
        let note = if *failing == "dm" {
            "; node dm-node (dm) is left drained"
        } else {
            ""
        };
        assert_eq!(
            roll.hosts[at].detail,
            Some(format!("dpkg: lock held{note}"))
        );
        assert_eq!(
            rig.log.touching(failing).last(),
            Some(&e(&format!("in:{failing}")))
        );
    }
}

/// Catches a bound taken from the wrong role: storage nodes and the frontend wait 300 s, the
/// daemon 780 s, each ending in `health_timeout` with the last thing the host said.
#[test]
fn the_health_bound_follows_the_role() {
    for (alias, bound) in [("fe", 300), ("st1", 300), ("dm", 780)] {
        let rig = Rig::new(&format!("bound-{alias}"));
        rig.health.script(
            alias,
            vec![],
            Ok(Health::Waiting("commit abc is not running yet".into())),
        );
        let u = rig.open();
        let roll = rig.roll(&u);
        assert_eq!(failure_code(&roll), ErrorCode::HealthTimeout, "{alias}");
        assert!(roll.failure.expect("f").detail.contains("not running yet"));
        assert_eq!(rig.elapsed(), Duration::from_secs(bound), "{alias}");
    }
}

/// Catches an abandoned swap waited out to the bound instead of failing at once, and a
/// transient health error ending a roll that would have recovered.
#[test]
fn an_abandoned_swap_fails_at_once_and_a_blip_is_waited_out() {
    let rig = Rig::new("abandoned");
    rig.health.script(
        "fe",
        vec![],
        Ok(Health::Abandoned("the swap was reverted".into())),
    );
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(failure_code(&roll), ErrorCode::SwapAbandoned);
    assert_eq!(rig.elapsed(), Duration::ZERO);
    assert_eq!(rig.log.installs(), ["fe"]);

    let rig = Rig::new("blip");
    rig.health.script(
        "st1",
        vec![
            Err(PortError("timeout".into())),
            Ok(Health::Waiting("starting".into())),
        ],
        Ok(Health::Healthy),
    );
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Done);
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Catches port text reaching the roll raw: a newline in a wrapper's refusal must not
/// survive into a record that is logged line by line and served on GET.
#[test]
fn port_text_is_one_short_line_in_the_roll() {
    let rig = Rig::new("clean-detail");
    let nasty = format!("refused\nSECOND LINE\x1b[31m{}", "x".repeat(400));
    rig.inst.script_install("fe", vec![], Err(PortError(nasty)));
    let u = rig.open();
    let roll = rig.roll(&u);
    let detail = roll.failure.expect("f").detail;
    assert!(!detail.contains('\n') && !detail.contains('\x1b'));
    assert_eq!(detail.chars().count(), 200);
}

/// Catches a second roll starting beside the first: while one runs, a different key is a 409
/// `roll_active` that names it, nothing is written for the second, and once the first is done
/// the next starts with the next id.
#[test]
fn a_second_roll_is_409_while_one_is_active() {
    let rig = Rig::new("active");
    let u = rig.open();
    let first = u.start(&all("2.0", "run-1")).expect("first").roll;
    let err = u.start(&all("2.0", "run-2")).expect_err("409");
    assert_eq!((err.code, err.code.status()), (ErrorCode::RollActive, 409));
    assert_eq!(err.roll_id.as_deref(), Some("roll-1"));
    assert_eq!(
        u.current().expect("live").expect("current"),
        first,
        "the second left no trace"
    );

    u.drive("roll-1").expect("drive");
    let second = u.start(&all("2.1", "run-2")).expect("second");
    assert!(second.created);
    assert_eq!(second.roll.id, "roll-2");
    assert_eq!(second.roll.state, RollState::Running);
    assert_eq!(
        u.get("roll-1").expect_err("replaced").code,
        ErrorCode::UnknownRoll
    );
}

/// Catches a failed roll leaving the updater stuck (a failed roll is over; the next may begin)
/// and an aborted one too.
#[test]
fn a_failed_or_aborted_roll_does_not_block_the_next() {
    let rig = Rig::new("after-failed");
    rig.inst
        .script_install("fe", vec![Err(PortError("no".into()))], Ok(()));
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Failed);
    let next = u.start(&all("2.0", "run-2")).expect("after failed").roll;
    assert_eq!(u.abort(&next.id).expect("abort").state, RollState::Aborted);
    assert!(u.start(&all("2.0", "run-3")).is_ok(), "after aborted");
}

/// Catches an idempotency key that is not idempotent: the same key and body returns the same
/// roll (200, not created) whether the roll is running or done, and no second record or
/// second set of installs appears.
#[test]
fn the_same_key_and_body_is_the_same_roll() {
    let rig = Rig::new("idem");
    let u = rig.open();
    let req = all("2.0", "run-77");
    let first = u.start(&req).expect("first");
    assert!(first.created);
    let again = u.start(&req).expect("again");
    assert!(!again.created);
    assert_eq!(again.roll, first.roll);
    let done = u.drive(&first.roll.id).expect("drive");
    let after = u.start(&req).expect("after done");
    assert_eq!((after.created, after.roll), (false, done));
    assert_eq!(rig.log.installs().len(), 4, "the replay installed nothing");
    // A fresh process answers the replay too: the key is in the record.
    let reopened = rig.open();
    assert!(!reopened.start(&req).expect("replay").created);
}

/// Catches a key reused for something else being silently served the old roll: a different
/// version or host set under the same key is an error, even while the roll is active (the key
/// check comes before the one-roll rule).
#[test]
fn the_same_key_with_a_different_body_is_an_error() {
    let rig = Rig::new("idem-conflict");
    let u = rig.open();
    u.start(&all("2.0", "run-5")).expect("first");
    for other in [
        all("2.1", "run-5"),
        request("2.0", Hosts::Aliases(vec!["fe".into()]), "run-5"),
    ] {
        let err = u.start(&other).expect_err("conflict");
        assert_eq!(err.code, ErrorCode::KeyReused);
        assert_eq!(err.code.status(), 409);
        assert_eq!(err.roll_id.as_deref(), Some("roll-1"));
    }
    assert_eq!(u.current().expect("live").expect("c").id, "roll-1");
}

/// Catches an unknown alias getting past the door: it is a 400 before any record is written
/// or any port is called, and the updater can still start a good roll after.
#[test]
fn an_unknown_alias_is_a_400_before_anything_runs() {
    let rig = Rig::new("bad-alias");
    let u = rig.open();
    let hosts = Hosts::Aliases(vec!["fe".into(), "nope".into()]);
    let err = u.start(&request("2.0", hosts, "k")).expect_err("400");
    assert_eq!(
        (err.code, err.code.status()),
        (ErrorCode::UnknownAlias, 400)
    );
    assert_eq!(u.current().expect("live"), None);
    assert!(rig.log.events().is_empty());
    assert!(
        rig.store().load().expect("load").is_none(),
        "no roll.json was written"
    );
    assert!(u.start(&all("2.0", "k")).is_ok());
}

/// Catches `drive` and `get` mishandling ids: unknown ids are 404, a finished roll is returned
/// as it is without running anything, and a roll driven twice at once is refused (a second
/// driver would install twice) while `get` and `current` still answer from inside the first.
#[test]
fn drive_and_get_by_id_and_a_second_driver_is_refused() {
    let rig = Rig::new("drive-id");
    let u = Arc::new(rig.open());
    assert_eq!(
        u.drive("roll-9").expect_err("none").code,
        ErrorCode::UnknownRoll
    );
    assert_eq!(
        u.get("roll-9").expect_err("none").code,
        ErrorCode::UnknownRoll
    );
    assert_eq!(u.current().expect("live"), None);
    let started = u.start(&all("2.0", "k")).expect("start");
    assert_eq!(
        u.drive("roll-9").expect_err("other id").code,
        ErrorCode::UnknownRoll,
        "a roll exists, but not this one"
    );

    let inside = Arc::new(std::sync::Mutex::new(None));
    let (u2, sink) = (Arc::clone(&u), Arc::clone(&inside));
    let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    rig.inst.on_install(move |h| {
        if h.alias == "fe" {
            // Without the guard the second drive installs `fe` again and lands here again,
            // for ever: fail on the second entry instead of hanging.
            let n = entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                n, 0,
                "the second driver was let in and re-entered the install"
            );
            let second = u2.drive("roll-1").expect_err("second driver");
            let seen = u2.get("roll-1").expect("get while driving");
            *sink.lock().expect("sink") = Some((second, seen.state));
        }
    });
    let done = u.drive(&started.roll.id).expect("drive");
    let (second, state) = inside.lock().expect("sink").take().expect("hook ran");
    assert_eq!(
        (second.code, second.roll_id.as_deref()),
        (ErrorCode::RollActive, Some("roll-1"))
    );
    assert_eq!(state, RollState::Running);

    let before = rig.log.events();
    assert_eq!(u.drive("roll-1").expect("finished"), done);
    assert_eq!(
        rig.log.events(),
        before,
        "driving a finished roll runs nothing"
    );
}

/// Catches an abort that stops mid-host or does not stop at all: asked while host `fe` is
/// installing, the roll finishes `fe` (a host is never left half-installed on purpose), then
/// stops as `aborted` with every later host pending and untouched; abort of it again is 409.
#[test]
fn an_abort_during_a_roll_takes_effect_between_hosts() {
    let rig = Rig::new("abort-running");
    let u = Arc::new(rig.open());
    let id = u.start(&all("2.0", "k")).expect("start").roll.id;
    let (u2, id2) = (Arc::clone(&u), id.clone());
    rig.inst.on_install(move |h| {
        if h.alias == "fe" {
            let r = u2.abort(&id2).expect("abort");
            assert_eq!(r.state, RollState::Running, "takes effect after the host");
        }
    });
    let roll = u.drive(&id).expect("drive");
    assert_eq!(roll.state, RollState::Aborted);
    assert_eq!(
        phases(&roll),
        [
            ("fe".into(), Phase::Done),
            ("st1".into(), Phase::Pending),
            ("dm".into(), Phase::Pending),
            ("st2".into(), Phase::Pending)
        ]
    );
    assert_eq!(rig.log.installs(), ["fe"]);
    assert_eq!(
        u.abort(&id).expect_err("terminal").code,
        ErrorCode::RollTerminal
    );
    assert_eq!(rig.store().load().expect("load").expect("rec").roll, roll);
}

/// Catches abort of a roll nobody drives not taking effect, and abort of an unknown id (the
/// abort during the last host is in `updater_followup_tests`).
#[test]
fn an_idle_roll_aborts_at_once() {
    let rig = Rig::new("abort-idle");
    let idle = rig.open();
    let next = idle.start(&all("2.0", "k2")).expect("start").roll.id;
    assert_eq!(
        idle.abort("roll-99").expect_err("none").code,
        ErrorCode::UnknownRoll
    );
    assert_eq!(idle.abort(&next).expect("abort").state, RollState::Aborted);
    assert_eq!(idle.drive(&next).expect("drive").state, RollState::Aborted);
}

/// Catches a record written after the work instead of before it: when the installer is
/// called, `roll.json` already says `installing` with the attempt counted, so a crash inside
/// the install is found as `installing` and asked about, not repeated blindly.
#[test]
fn installing_is_on_disk_before_the_install_starts() {
    let rig = Rig::new("write-ahead");
    let u = rig.open();
    let store = rig.store();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    rig.inst.on_install(move |h| {
        let rec = store.load().expect("load").expect("record");
        let host = rec
            .roll
            .hosts
            .iter()
            .find(|x| x.alias == h.alias)
            .expect("host");
        sink.lock()
            .expect("sink")
            .push((h.alias.clone(), host.phase, host.attempts));
    });
    rig.roll(&u);
    let seen = seen.lock().expect("sink").clone();
    assert_eq!(seen.len(), 4);
    assert!(
        seen.iter()
            .all(|(_, p, a)| *p == Phase::Installing && *a == 1),
        "{seen:?}"
    );
}

/// Catches a roll started on a record that cannot be written: the start is `state_lost`
/// (503), leaves no roll behind and calls no port. (A write that fails mid-roll is in the
/// crash matrix of the restart tests.)
#[test]
fn a_record_that_cannot_be_written_stops_the_updater() {
    let rig = Rig::new("unwritable");
    // The directory is gone: every write fails.
    let gone = rig.dir.join("missing");
    let u = rig.open_with(RollStore::with_sync(&gone, Box::new(Fsync)));
    let err = u.start(&all("2.0", "k")).expect_err("state lost");
    assert_eq!((err.code, err.code.status()), (ErrorCode::StateLost, 503));
    assert_eq!(u.memory(), None);
    assert!(rig.log.events().is_empty());
}

/// Catches the shape of GET drifting from what the workflow reads: id, state, hosts with
/// alias, phase, attempts and detail, and `failure` null on success.
#[test]
fn the_roll_serializes_in_the_documented_shape() {
    let rig = Rig::new("shape");
    let u = rig.open();
    let started = u
        .start(&request("2.0", Hosts::Aliases(vec!["fe".into()]), "k"))
        .expect("s");
    assert_eq!(
        serde_json::to_string(&started.roll).expect("json"),
        "{\"id\":\"roll-1\",\"state\":\"running\",\"hosts\":[{\"alias\":\"fe\",\"phase\":\"pending\",\"attempts\":0,\"detail\":null}],\"failure\":null}"
    );
}
