//! The roll API over a real unix socket in a scratch directory, with the fakes behind it.
//! Each test's doc comment says the behaviour it asserts.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{ErrorCode, Roll, RollState};
use crate::daemon::{Config, Daemon, Stopper, start_with};
use crate::fakes::{
    Event, FakeClock, FakeHealth, FakeInstaller, FakeSafeToStop, Log, unsafe_because,
};
use crate::listener::{Listen, bind_socket};
use crate::server::{Recent, Server, Settings, peer_allowed};
use crate::testutil::{
    KEY, Lines, call, eventually, exchange, file, locked, own_uid, parsed, raw, scratch, sock,
};
use crate::{Ports, Timing};

const INVENTORY: &str = "fe fe.farm.example frontend KEY\nst1 st1.farm.example storage KEY\n\
                         dm dm.farm.example daemon KEY dm-node\n";

/// A scratch directory with an inventory and an empty state directory, and the config over it.
fn prepare(name: &str, proxy_uid: u32) -> Config {
    let dir = scratch(name);
    let inventory = file(&dir, "inventory", &INVENTORY.replace("KEY", KEY), 0o644);
    let state = dir.join("state");
    std::fs::create_dir(&state).expect("state dir");
    Config {
        state_dir: state,
        inventory,
        inventory_owner: own_uid(),
        listen: Listen::Bind(dir.join("api.sock")),
        proxy_uid,
        operator_api: "127.0.0.1:1".parse().expect("addr"),
        token_file: dir.join("unused"),
        ssh_key_file: dir.join("unused-key"),
        ssh_user: "kbf".to_owned(),
        channel: "stable".to_owned(),
        ssh_connect_timeout: std::time::Duration::from_secs(5),
        ssh_command_timeout: std::time::Duration::from_secs(30),
        drain_deadline: std::time::Duration::from_secs(600),
        apply_poll: std::time::Duration::from_secs(5),
        apply_deadline: std::time::Duration::from_secs(600),
    }
}

/// A running updater over fakes, on a socket.
struct Farm {
    socket: PathBuf,
    log: Log,
    stop: FakeSafeToStop,
    inst: FakeInstaller,
    audit: Lines,
    stopper: Stopper,
    daemon: Option<Daemon>,
}

impl Farm {
    fn new(name: &str, proxy_uid: u32) -> Farm {
        Farm::with(name, proxy_uid, |s| s)
    }

    fn with(name: &str, proxy_uid: u32, tune: impl FnOnce(Settings) -> Settings) -> Farm {
        Farm::launch(&prepare(name, proxy_uid), tune)
    }

    fn launch(cfg: &Config, tune: impl FnOnce(Settings) -> Settings) -> Farm {
        Farm::launch_on(cfg, sock(cfg), tune)
    }

    /// [`Farm::launch`] where the socket's path is `socket` (the config may name a
    /// descriptor instead).
    fn launch_on(cfg: &Config, socket: PathBuf, tune: impl FnOnce(Settings) -> Settings) -> Farm {
        let log = Log::default();
        let (stop, inst, health, clock) = (
            FakeSafeToStop::new(&log),
            FakeInstaller::new(&log),
            FakeHealth::new(&log),
            FakeClock::new(),
        );
        let audit = Lines::default();
        let ports = Ports {
            safe: Box::new(stop.clone()),
            installer: Box::new(inst.clone()),
            drain: Box::new(crate::fakes::FakeDrain::new(&log)),
            health: Box::new(health),
            clock: Box::new(clock),
        };
        let settings = tune(Settings::new(cfg.proxy_uid));
        let daemon =
            start_with(cfg, ports, Timing::default(), settings, audit.sink()).expect("start");
        Farm {
            socket,
            log,
            stop,
            inst,
            audit,
            stopper: daemon.stopper(),
            daemon: Some(daemon),
        }
    }

    fn post(&self, version: &str, key: &str) -> (u16, Value) {
        let body = json!({"package": "kbf", "version": version, "idempotency_key": key});
        call(&self.socket, "POST", "/v1/updater/rolls", &body.to_string())
    }

    fn get(&self, id: &str) -> (u16, Value) {
        call(&self.socket, "GET", &format!("/v1/updater/rolls/{id}"), "")
    }

    fn abort(&self, id: &str) -> (u16, Value) {
        call(
            &self.socket,
            "POST",
            &format!("/v1/updater/rolls/{id}/abort"),
            "",
        )
    }

    /// Poll until the roll leaves `running`, and return it.
    fn finished(&self, id: &str) -> Roll {
        let mut last = None;
        eventually(|| {
            let (status, body) = self.get(id);
            assert_eq!(status, 200);
            let roll: Roll = serde_json::from_value(body).expect("a roll");
            let done = roll.state != RollState::Running;
            last = Some(roll);
            done
        });
        last.expect("a roll")
    }

    fn current_rolls(&self) -> Value {
        call(&self.socket, "GET", "/v1/updater/rolls", "").1
    }
}

impl Drop for Farm {
    fn drop(&mut self) {
        self.stopper.stop();
        if let Some(d) = self.daemon.take() {
            d.wait();
        }
    }
}

fn codes(v: &Value) -> ErrorCode {
    serde_json::from_value(v["code"].clone()).expect("a code of the closed set")
}

/// Catches the whole path being broken somewhere between the socket and the state machine:
/// a POST is a 202 with an id, the roll is driven in the plan's order (frontend first, each
/// host once), a poll sees it done with every host done, and the list shows it.
#[test]
fn a_roll_runs_end_to_end_over_the_socket() {
    let farm = Farm::new("e2e", own_uid());
    let (status, body) = farm.post("2.0-1", "run-1");
    assert_eq!((status, &body["state"]), (202, &json!("running")), "{body}");
    let id = body["roll_id"].as_str().expect("id").to_owned();
    let roll = farm.finished(&id);
    assert_eq!(roll.state, RollState::Done, "{roll:?}");
    let aliases: Vec<_> = roll.hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(aliases, ["fe", "st1", "dm"]);
    assert_eq!(farm.log.installs(), ["fe", "st1", "dm"]);
    // The list: the latest roll is `current`; a second roll pushes the first into `recent`.
    let list = farm.current_rolls();
    assert_eq!(
        (
            list["current"]["id"].as_str(),
            list["recent"].as_array().map(Vec::len)
        ),
        (Some("roll-1"), Some(0))
    );
    let second = farm.post("2.1-1", "run-2").1["roll_id"]
        .as_str()
        .expect("id")
        .to_owned();
    farm.finished(&second);
    let list = farm.current_rolls();
    assert_eq!(list["current"]["id"], "roll-2");
    assert_eq!(list["recent"][0]["id"], "roll-1");
    assert_eq!(list["recent"].as_array().map(Vec::len), Some(1));
    // Before any roll, current is null.
    let empty = Farm::new("empty", own_uid());
    assert_eq!(
        empty.current_rolls(),
        json!({"current": null, "recent": []})
    );
}

/// Catches an idempotency key that starts a second roll (a re-run of the workflow would
/// install twice), a repeat that answers 202, and a changed body under the same key that is
/// not a `key_reused` 409 naming the roll.
#[test]
fn the_same_key_is_the_same_roll_and_a_changed_body_is_409() {
    let farm = Farm::new("idem", own_uid());
    let (status, first) = farm.post("2.0-1", "run-7");
    assert_eq!(status, 202);
    let id = first["roll_id"].as_str().expect("id").to_owned();
    farm.finished(&id);
    let installs = farm.log.installs();
    let (status, again) = farm.post("2.0-1", "run-7");
    assert_eq!(
        (status, again["roll_id"].as_str()),
        (200, Some(id.as_str())),
        "{again}"
    );
    assert_eq!(again["state"], "done");
    assert_eq!(
        farm.log.installs(),
        installs,
        "a repeat must not install again"
    );
    let (status, other) = farm.post("9.9-1", "run-7");
    assert_eq!(
        (status, codes(&other)),
        (409, ErrorCode::KeyReused),
        "{other}"
    );
    assert_eq!(other["roll_id"], id.as_str());
}

/// Catches a second roll started while one runs (409 `roll_active` naming the running roll),
/// an abort that is not accepted while the roll is driven, an abort that does not stop the
/// roll between hosts, and a second abort of a finished roll that is not 409 `roll_terminal`.
#[test]
fn a_second_roll_is_refused_while_one_runs_and_an_abort_stops_it() {
    let farm = Farm::new("busy", own_uid());
    let (entered_tx, entered) = channel::<()>();
    let (release, released) = channel::<()>();
    let (entered_tx, released) = (Mutex::new(entered_tx), Mutex::new(released));
    farm.inst.on_install(move |host| {
        if host.alias == "fe" {
            locked(&entered_tx).send(()).expect("entered");
            locked(&released).recv().expect("release");
        }
    });
    let id = farm.post("2.0-1", "run-1").1["roll_id"]
        .as_str()
        .expect("id")
        .to_owned();
    entered
        .recv_timeout(Duration::from_secs(20))
        .expect("the install began");
    let (status, busy) = farm.post("2.1-1", "run-2");
    assert_eq!(
        (status, codes(&busy)),
        (409, ErrorCode::RollActive),
        "{busy}"
    );
    assert_eq!(busy["roll_id"], id.as_str());
    // The abort is accepted while the frontend install is in hand; the roll ends before st1.
    let (status, body) = farm.abort(&id);
    assert_eq!((status, &body["state"]), (200, &json!("running")), "{body}");
    release.send(()).expect("release");
    let roll = farm.finished(&id);
    assert_eq!(roll.state, RollState::Aborted);
    assert_eq!(farm.log.installs(), ["fe"], "no host after the abort");
    let (status, again) = farm.abort(&id);
    assert_eq!(
        (status, codes(&again)),
        (409, ErrorCode::RollTerminal),
        "{again}"
    );
    // A roll that is over lets the next one start.
    assert_eq!(farm.post("2.1-1", "run-2").0, 202);
}

/// Catches a roll the cluster says is unsafe being stopped on, and a failed roll being a 500
/// or hidden: the poll sees `failed`, the failure's closed code, and no later host touched.
#[test]
fn a_failed_roll_reads_as_failed_with_a_code() {
    let farm = Farm::new("fail", own_uid());
    farm.stop
        .script("st1", vec![], Ok(unsafe_because("m2 owes 5 blobs")));
    let id = farm.post("2.0-1", "run-1").1["roll_id"]
        .as_str()
        .expect("id")
        .to_owned();
    let roll = farm.finished(&id);
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(
        (failure.alias.as_str(), failure.code),
        ("st1", ErrorCode::UnsafeToStop)
    );
    assert_eq!(farm.log.touching("dm"), Vec::<Event>::new());
}

/// Catches an unknown or malformed roll id answering anything but 404 `unknown_roll`, and an
/// id echoed back (it is the caller's text in a JSON body and in the audit line).
#[test]
fn an_unknown_roll_is_404_and_the_text_is_not_echoed() {
    let farm = Farm::new("unknown", own_uid());
    farm.post("2.0-1", "run-1");
    for id in [
        "roll-99",
        "roll-",
        "roll-1x",
        "nonsense<script>",
        "roll-123456789012345678901",
    ] {
        for (status, body) in [farm.get(id), farm.abort(id)] {
            assert_eq!(
                (status, codes(&body)),
                (404, ErrorCode::UnknownRoll),
                "{id}"
            );
            assert!(!body.to_string().contains("script"), "{body}");
        }
    }
}

/// Catches routes that answer when they should not: a prefix (`/v1/updater`, `/v1/updaterx`),
/// a trailing slash, a query string on a collection, and the wrong method on a real route is
/// 405, not 404 and not served.
#[test]
fn only_the_four_routes_exist_and_methods_are_checked() {
    let farm = Farm::new("routes", own_uid());
    for path in [
        "/",
        "/v1/updater",
        "/v1/updaterx/rolls",
        "/v1/updater/rolls?x=1",
        "/v1/updater/rolls/roll-1/abort/more",
        "/v1/updater/rolls/roll-1/other",
        "/v1/other",
    ] {
        let (status, body) = call(&farm.socket, "GET", path, "");
        assert_eq!((status, codes(&body)), (404, ErrorCode::NotFound), "{path}");
    }
    // An empty id is no roll.
    assert_eq!(call(&farm.socket, "GET", "/v1/updater/rolls/", "").0, 404);
    for (method, path) in [
        ("DELETE", "/v1/updater/rolls"),
        ("PUT", "/v1/updater/rolls"),
        ("POST", "/v1/updater/rolls/roll-1"),
        ("DELETE", "/v1/updater/rolls/roll-1"),
        ("GET", "/v1/updater/rolls/roll-1/abort"),
    ] {
        let (status, body) = call(&farm.socket, method, path, "");
        assert_eq!(
            (status, codes(&body)),
            (405, ErrorCode::MethodNotAllowed),
            "{method} {path}"
        );
    }
    assert_eq!(farm.log.events(), Vec::<Event>::new(), "nothing ran");
}

/// Catches the trust boundary leaking: a request with an Authorization header (any case)
/// must be refused with nothing started, the token must not reach the audit log, and an
/// identity header sent twice (a forged one beside the proxy's) must be refused too.
#[test]
fn credentials_and_repeated_identity_headers_are_refused() {
    let farm = Farm::new("trust", own_uid());
    let body = json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"}).to_string();
    for name in ["Authorization", "authorization", "AUTHORIZATION"] {
        let r = raw(
            "POST",
            "/v1/updater/rolls",
            &[(name, "Bearer hunter2-secret")],
            &body,
        );
        let (status, answer) = parsed(&exchange(&farm.socket, r.as_bytes()));
        assert_eq!(
            (status, codes(&answer)),
            (400, ErrorCode::CredentialsPresent),
            "{name}"
        );
        assert!(!answer.to_string().contains("hunter2"));
    }
    let twice = raw(
        "POST",
        "/v1/updater/rolls",
        &[("X-Kbf-Jti", "a"), ("x-kbf-jti", "b")],
        &body,
    );
    let (status, answer) = parsed(&exchange(&farm.socket, twice.as_bytes()));
    assert_eq!((status, codes(&answer)), (400, ErrorCode::BadRequest));
    // Even a GET with a credential is refused.
    let get = raw(
        "GET",
        "/v1/updater/rolls",
        &[("Authorization", "Basic abc")],
        "",
    );
    assert_eq!(parsed(&exchange(&farm.socket, get.as_bytes())).0, 400);
    assert_eq!(
        farm.current_rolls()["current"],
        Value::Null,
        "no roll was started"
    );
    let lines = farm.audit.events("request", 5);
    assert!(
        !farm.audit.raw().contains("hunter2"),
        "the credential must not be logged"
    );
    assert!(
        lines
            .iter()
            .take(3)
            .all(|l| l["code"] == "credentials_present")
    );
}

/// Catches the audit trail missing the proxy's identity: one line per request, with the four
/// `x-kbf-*` values, the peer, the status and the code; long values cut; nothing else from
/// the request (a header the proxy did not set is not copied).
#[test]
fn every_request_is_one_audit_line_with_the_proxys_identity() {
    let farm = Farm::new("audit", own_uid());
    let long = "x".repeat(1000);
    let headers = [
        ("X-Kbf-Jti", "jti-1"),
        ("X-Kbf-Job-Workflow-Ref", long.as_str()),
        ("X-Kbf-Ref", "refs/heads/main"),
        ("X-Kbf-Sha", "abc123"),
        ("X-Other", "not-recorded"),
    ];
    let body = json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"}).to_string();
    let r = raw("POST", "/v1/updater/rolls", &headers, &body);
    let (status, _) = parsed(&exchange(&farm.socket, r.as_bytes()));
    assert_eq!(status, 202);
    let lines = farm.audit.events("request", 1);
    let line = &lines[0];
    assert_eq!(line["jti"], "jti-1");
    assert_eq!(line["ref"], "refs/heads/main");
    assert_eq!(line["sha"], "abc123");
    assert_eq!(line["job_workflow_ref"].as_str().map(str::len), Some(256));
    assert_eq!(
        (line["method"].as_str(), line["path"].as_str()),
        (Some("POST"), Some("/v1/updater/rolls"))
    );
    assert_eq!(
        (line["status"].as_u64(), line["peer_uid"].as_u64()),
        (Some(202), Some(u64::from(own_uid())))
    );
    assert_eq!(line["roll_id"], "roll-1");
    assert_eq!(line["code"], Value::Null);
    assert!(!farm.audit.raw().contains("not-recorded"));
    // A refusal is a line too, with its code, and no identity if none was sent.
    let (status, _) = call(&farm.socket, "GET", "/nope", "");
    assert_eq!(status, 404);
    let lines = farm.audit.events("request", 2);
    assert_eq!(
        (lines[1]["code"].as_str(), lines[1]["jti"].clone()),
        (Some("not_found"), Value::Null)
    );
    // A finished roll is a line as well.
    let done = farm.audit.events("roll_finished", 1);
    assert_eq!(done[0]["roll_id"], "roll-1");
}

/// Catches the body cap: a declared length over 4096 is refused with 413 before any body is
/// read (the test sends none), a body of exactly 4096 is served, and nothing is started by
/// the refused one.
#[test]
fn the_body_cap_is_4096_bytes() {
    let farm = Farm::new("cap", own_uid());
    let over = "POST /v1/updater/rolls HTTP/1.1\r\nContent-Length: 4097\r\n\r\n";
    let (status, body) = parsed(&exchange(&farm.socket, over.as_bytes()));
    assert_eq!((status, codes(&body)), (413, ErrorCode::BodyTooLarge));
    assert_eq!(farm.current_rolls()["current"], Value::Null);
    let json = json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"}).to_string();
    let padded = format!("{json}{}", " ".repeat(4096 - json.len()));
    let (status, _) = call(&farm.socket, "POST", "/v1/updater/rolls", &padded);
    assert_eq!(status, 202);
}

/// Catches an error that leaks the inventory or comes from outside the closed set: every
/// refusal of a bad request is one of the listed codes, and an unknown alias names only what
/// the caller sent, not the aliases or addresses of the inventory.
#[test]
fn bad_requests_get_closed_codes_and_no_inventory() {
    let farm = Farm::new("closed", own_uid());
    let cases = [
        ("{", ErrorCode::BadRequest),
        ("{\"package\":\"kbf\"}", ErrorCode::BadRequest),
        (
            "{\"package\":\"kbf\",\"version\":\"-1\",\"idempotency_key\":\"k\"}",
            ErrorCode::BadVersion,
        ),
        (
            "{\"package\":\"kbf\",\"version\":\"1\",\"idempotency_key\":\"k\",\"hosts\":[\"nope\"]}",
            ErrorCode::UnknownAlias,
        ),
        (
            "{\"package\":\"kbf\",\"version\":\"1\",\"idempotency_key\":\"k\",\"url\":\"x\"}",
            ErrorCode::BadRequest,
        ),
    ];
    for (body, want) in cases {
        let (status, answer) = call(&farm.socket, "POST", "/v1/updater/rolls", body);
        assert_eq!((status, codes(&answer)), (400, want), "{body}");
        let text = answer.to_string();
        for secret in [
            "fe.farm",
            "st1",
            "dm.farm",
            "ssh-ed25519",
            "storage",
            "frontend",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
    assert_eq!(farm.current_rolls()["current"], Value::Null);
}

/// Catches a peer that is not the proxy being served: the connection is closed with nothing
/// written and nothing read, nothing is started, and the audit line names the uid. The uid is
/// the kernel's (`SO_PEERCRED`): the test's own, configured as "not the proxy".
#[test]
fn a_peer_that_is_not_the_proxy_gets_nothing() {
    let farm = Farm::new("peer", own_uid() + 1);
    let body = json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"}).to_string();
    let answer = exchange(
        &farm.socket,
        raw("POST", "/v1/updater/rolls", &[], &body).as_bytes(),
    );
    assert!(answer.is_empty(), "{answer:?}");
    assert_eq!(parsed(&answer), (0, Value::Null));
    let lines = farm.audit.events("peer_refused", 1);
    assert_eq!(lines[0]["peer_uid"].as_u64(), Some(u64::from(own_uid())));
    assert_eq!(farm.log.events(), Vec::<Event>::new());
    assert!(farm.audit.all().iter().all(|l| l["event"] != "request"));
}

/// Catches the peer check accepting an unknown peer or a different uid, with fakes (a second
/// real uid needs root).
#[test]
fn the_peer_check_wants_the_proxy_uid_exactly() {
    assert!(peer_allowed(Some(993), 993));
    assert!(!peer_allowed(Some(0), 993), "root is not the proxy");
    assert!(!peer_allowed(Some(994), 993));
    assert!(!peer_allowed(None, 993), "an unknown peer is refused");
    assert!(!peer_allowed(None, 0));
}

/// Catches a slow or dead client holding the server: a client that sends half a head is let go
/// after the timeout and the audit says why; one that connects and says nothing likewise; a
/// full server closes the next connection unread, and serves again when a slot frees.
#[test]
fn slow_clients_time_out_and_the_connection_cap_holds() {
    let farm = Farm::with("slow", own_uid(), |mut s| {
        s.io_timeout = Duration::from_millis(300);
        s.max_connections = 1;
        s
    });
    let mut stalled = UnixStream::connect(&farm.socket).expect("connect");
    std::io::Write::write_all(&mut stalled, b"GET /v1/upd").expect("write");
    // The one slot is taken: the next connection is closed without an answer.
    eventually(|| {
        let answer = exchange(
            &farm.socket,
            raw("GET", "/v1/updater/rolls", &[], "").as_bytes(),
        );
        answer.is_empty()
            && !farm
                .audit
                .all()
                .iter()
                .all(|l| l["event"] != "connection_refused")
    });
    let lost = farm.audit.events("connection_lost", 1);
    assert!(lost[0]["why"].as_str().is_some_and(|w| !w.is_empty()));
    // The stalled connection is over, so a request is served again.
    eventually(|| call(&farm.socket, "GET", "/v1/updater/rolls", "").0 == 200);
}

/// A server over fakes with no thread behind it, and the queue its driver would read.
fn bare_server(name: &str) -> (Server, std::sync::mpsc::Receiver<String>, Lines) {
    let cfg = prepare(name, own_uid());
    let log = Log::default();
    let ports = Ports {
        safe: Box::new(FakeSafeToStop::new(&log)),
        installer: Box::new(FakeInstaller::new(&log)),
        drain: Box::new(crate::fakes::FakeDrain::new(&log)),
        health: Box::new(FakeHealth::new(&log)),
        clock: Box::new(FakeClock::new()),
    };
    let updater = crate::Updater::open(
        crate::inventory::Inventory::load(&cfg.inventory, own_uid()).expect("inventory"),
        crate::record::RollStore::open(&cfg.state_dir).expect("store"),
        ports,
        Timing::default(),
    )
    .expect("open");
    let (tx, rx): (Sender<String>, _) = channel();
    let audit = Lines::default();
    let server = Server::new(
        Arc::new(updater),
        Settings::new(own_uid()),
        audit.sink(),
        tx,
        Arc::new(Recent::default()),
    );
    (server, rx, audit)
}

/// POST the roll `{version 2.0, key k}` to `server` over a socket pair; the status.
fn post_k(server: &Server) -> u16 {
    let (ours, theirs) = UnixStream::pair().expect("pair");
    let body = json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"}).to_string();
    std::io::Write::write_all(
        &mut &ours,
        raw("POST", "/v1/updater/rolls", &[], &body).as_bytes(),
    )
    .expect("write");
    server.connection(theirs);
    let mut answer = Vec::new();
    std::io::Read::read_to_end(&mut &ours, &mut answer).expect("read");
    parsed(&answer).0
}

/// Catches a driver that has gone away being silent: the roll is recorded, the reply is
/// still 202, and the audit says that nothing will run it.
#[test]
fn a_dead_driver_is_audited() {
    let (server, rx, audit) = bare_server("nodriver");
    drop(rx);
    assert_eq!(post_k(&server), 202);
    assert_eq!(audit.events("driver_gone", 1)[0]["roll_id"], "roll-1");
}

/// Catches a replayed POST (same key and body: 200, `created` false) queueing the roll for
/// the driver again. A driver handed a roll it already ran would at best be told it is over;
/// the queue is where it shows, so it is read directly: the first POST queues `roll-1`, the
/// replay queues nothing, and a driver that is gone is not audited for the replay.
#[test]
fn a_replayed_post_queues_nothing_for_the_driver() {
    let (server, rx, audit) = bare_server("replay");
    assert_eq!(post_k(&server), 202);
    assert_eq!(rx.try_recv().as_deref(), Ok("roll-1"));
    assert_eq!(post_k(&server), 200);
    assert_eq!(post_k(&server), 200);
    assert!(rx.try_recv().is_err(), "a replay queued a roll");
    drop(rx);
    assert_eq!(post_k(&server), 200);
    assert!(audit.all().iter().all(|l| l["event"] != "driver_gone"));
}

/// Catches a sender that stays under the per-read timeout with every byte holding a
/// connection for ever: a valid request sent a byte at a time, each well inside the read
/// timeout, is cut off by the deadline on the whole request, and the audit says so; a
/// deadline of zero cuts it off before the first read. A request that arrives in time is
/// still served.
#[test]
fn a_slow_sender_is_cut_off_by_the_request_deadline() {
    let farm = Farm::with("deadline", own_uid(), |mut s| {
        s.io_timeout = Duration::from_secs(5);
        s.request_deadline = Duration::from_millis(300);
        s
    });
    let request = raw("GET", "/v1/updater/rolls", &[], "");
    let mut slow = UnixStream::connect(&farm.socket).expect("connect");
    for byte in request.as_bytes() {
        if std::io::Write::write_all(&mut slow, &[*byte]).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let lost = farm.audit.events("connection_lost", 1);
    let why = lost[0]["why"].as_str().expect("why");
    assert!(why.contains("deadline"), "{why}");
    assert!(
        farm.audit.all().iter().all(|l| l["event"] != "request"),
        "the slow request was served"
    );
    // One that arrives whole, in time, is served.
    assert_eq!(call(&farm.socket, "GET", "/v1/updater/rolls", "").0, 200);
    let none = Farm::with("deadline0", own_uid(), |mut s| {
        s.request_deadline = Duration::ZERO;
        s
    });
    assert_eq!(call(&none.socket, "GET", "/v1/updater/rolls", "").0, 0);
    let lost = none.audit.events("connection_lost", 1);
    assert!(
        lost[0]["why"]
            .as_str()
            .is_some_and(|w| w.contains("deadline"))
    );
}

/// Catches a socket that systemd bound being touched or mistaken: with an activated
/// descriptor the updater serves the socket as it was given (mode 0660, which it does not
/// change to 0600 or anything), answers on it, and does NOT remove the file when it stops;
/// the file belongs to the service manager.
#[test]
fn an_activated_socket_is_served_as_given_and_left_in_place() {
    use std::os::fd::IntoRawFd;
    let mut cfg = prepare("activated", own_uid());
    let path = sock(&cfg);
    let listener = UnixListener::bind(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).expect("chmod");
    cfg.listen = Listen::Activated(listener.into_raw_fd());
    let farm = Farm::launch_on(&cfg, path.clone(), |s| s);
    assert_eq!(call(&path, "GET", "/v1/updater/rolls", "").0, 200);
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
    assert_eq!(mode & 0o7777, 0o660, "the mode is the manager's");
    drop(farm);
    assert!(path.exists(), "the activated socket was removed");
}

/// Catches a roll left `running` by a crash not being resumed at start-up: a new process over
/// the same state directory runs it to the end without being asked.
#[test]
fn a_running_roll_found_at_start_is_resumed() {
    let cfg = prepare("resume", own_uid());
    {
        // The previous process: it accepted the roll and died before driving it.
        let log = Log::default();
        let ports = Ports {
            safe: Box::new(FakeSafeToStop::new(&log)),
            installer: Box::new(FakeInstaller::new(&log)),
            drain: Box::new(crate::fakes::FakeDrain::new(&log)),
            health: Box::new(FakeHealth::new(&log)),
            clock: Box::new(FakeClock::new()),
        };
        let updater = crate::Updater::open(
            crate::inventory::Inventory::load(&cfg.inventory, own_uid()).expect("inventory"),
            crate::record::RollStore::open(&cfg.state_dir).expect("store"),
            ports,
            Timing::default(),
        )
        .expect("open");
        let req = crate::api::RollRequest::from_json(
            json!({"package": "kbf", "version": "2.0", "idempotency_key": "k"})
                .to_string()
                .as_bytes(),
        )
        .expect("request");
        updater.start(&req).expect("start");
    }
    let farm = Farm::launch(&cfg, |s| s);
    let done = farm.audit.events("roll_finished", 1);
    assert_eq!(
        (done[0]["roll_id"].as_str(), done[0]["state"].as_str()),
        (Some("roll-1"), Some("done"))
    );
    assert_eq!(farm.log.installs(), ["fe", "st1", "dm"]);
}

/// Catches start-up treating any record as a roll to resume: a record that is already
/// finished stays as it is, no host is touched, and the next roll gets the next id.
#[test]
fn a_finished_roll_found_at_start_is_left_alone() {
    let cfg = prepare("finished", own_uid());
    let first = Farm::launch(&cfg, |s| s);
    let id = first.post("2.0-1", "run-1").1["roll_id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(first.finished(&id).state, RollState::Done);
    drop(first);
    let second = Farm::launch(&cfg, |s| s);
    let again = second.get(&id);
    assert_eq!((again.0, again.1["state"].as_str()), (200, Some("done")));
    assert_eq!(
        second.log.events(),
        Vec::<Event>::new(),
        "nothing is run again"
    );
    let next = second.post("2.1-1", "run-2");
    assert_eq!((next.0, next.1["roll_id"].as_str()), (202, Some("roll-2")));
}

/// Catches the socket being bound with the wrong mode or owner, or over something it should
/// not replace: mode 0600 and the process's own uid; a stale socket is replaced; a live one,
/// a regular file, a missing directory and a path with no file name are errors.
#[test]
fn the_socket_is_0600_and_binding_is_careful() {
    let dir = scratch("bind");
    let path = dir.join("api.sock");
    let live = bind_socket(&path).expect("bind");
    let meta = std::fs::symlink_metadata(&path).expect("meta");
    assert!(meta.file_type().is_socket());
    assert_eq!((meta.mode() & 0o7777, meta.uid()), (0o600, own_uid()));
    // A live socket is not taken over.
    assert!(
        bind_socket(&path)
            .expect_err("live")
            .to_string()
            .contains("listens")
    );
    drop(live);
    // The file stays after the listener is gone: stale, so replaced.
    assert!(std::fs::symlink_metadata(&path).is_ok());
    let again = bind_socket(&path).expect("stale");
    assert_eq!(
        std::fs::metadata(&path).expect("meta").permissions().mode() & 0o7777,
        0o600
    );
    drop(again);
    let plain = file(&dir, "plain", "x", 0o600);
    assert!(
        bind_socket(&plain)
            .expect_err("file")
            .to_string()
            .contains("not a socket")
    );
    assert!(bind_socket(&dir.join("missing/api.sock")).is_err());
    assert!(
        bind_socket(&dir.join("nope/.."))
            .expect_err("no name")
            .to_string()
            .contains("file name")
    );
    // The private directory it was bound in is gone after a bind, and after a failed one.
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        names.iter().all(|n| !n.to_string_lossy().starts_with('.')),
        "{names:?}"
    );
    let _ = UnixListener::bind(dir.join("other.sock")).expect("other");
}

/// Catches the list of recent rolls keeping an unfinished roll, repeating one, or growing
/// without bound.
#[test]
fn recent_keeps_finished_rolls_once_and_a_bounded_number() {
    let recent = Recent::default();
    let roll = |n: u32, state| Roll {
        id: format!("roll-{n}"),
        state,
        hosts: Vec::new(),
        failure: None,
    };
    recent.push(&roll(1, RollState::Running));
    recent.push(&roll(2, RollState::Interrupted));
    assert!(recent.list_except(None).is_empty());
    recent.push(&roll(3, RollState::Done));
    recent.push(&roll(3, RollState::Aborted));
    assert_eq!(recent.list_except(None).len(), 1);
    assert_eq!(recent.list_except(Some("roll-3")).len(), 0);
    for n in 10..40 {
        recent.push(&roll(n, RollState::Failed));
    }
    let list = recent.list_except(None);
    assert_eq!((list.len(), list[0].id.as_str()), (16, "roll-39"));
}
