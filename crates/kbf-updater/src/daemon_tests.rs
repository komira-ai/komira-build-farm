//! Flags, start-up failures, and the binary's own wiring (the real safe-to-stop and health
//! check, the ssh installer) over a real socket.

use std::os::unix::net::UnixListener;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{ErrorCode, Roll, RollState};
use crate::daemon::{
    Config, Daemon, build_ports, build_ports_with_exec, drive_loop, parse_flags,
    parse_operator_url, run_config, start, start_with, stderr_sink,
};
use crate::fakes::{Event, FakeClock, FakeHealth, FakeInstaller, FakeSafeToStop, Log};
use crate::listener::{Activation, Listen};
use crate::record::RollStore;
use crate::server::{Recent, Settings};
use crate::ssh_installer::{Exec, ExecError, Exited, Spawn};
use crate::testutil::{
    FakeApi, KEY, Lines, TOKEN, call, eventually, fake_api, file, http, locked, own_uid, scratch,
    sock,
};
use crate::{Ports, Timing, Updater, run_with};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

const FLAGS: [&str; 18] = [
    "--state-dir",
    "/s",
    "--inventory",
    "/i",
    "--socket",
    "/sock",
    "--proxy-uid",
    "993",
    "--operator-api",
    "http://127.0.0.1:8080",
    "--token-file",
    "/t",
    "--ssh-key-file",
    "/k",
    "--ssh-user",
    "kbf",
    "--channel",
    "stable",
];

fn parse(list: &[&str]) -> Result<Config, String> {
    parse_flags(&args(list), &Activation::default())
}

/// Catches a flag that is not read, read from the wrong place, or accepted twice, and a
/// required one being optional: the flags are required except the socket and the proxy
/// secret file, `--flag=value` works, an unknown argument and a flag with no value are errors,
/// and the inventory owner is root.
#[test]
fn the_flags_are_required_and_exact() {
    let cfg = parse(&FLAGS).expect("flags");
    assert_eq!(
        (
            cfg.state_dir.to_str(),
            cfg.inventory.to_str(),
            sock(&cfg).to_str(),
            cfg.token_file.to_str()
        ),
        (Some("/s"), Some("/i"), Some("/sock"), Some("/t"))
    );
    assert_eq!(
        (cfg.proxy_uid, cfg.inventory_owner, cfg.operator_api.port()),
        (993, 0, 8080)
    );
    assert_eq!(
        (
            cfg.ssh_key_file.to_str(),
            cfg.ssh_user.as_str(),
            cfg.channel.as_str()
        ),
        (Some("/k"), "kbf", "stable")
    );
    let eq: Vec<String> = FLAGS
        .chunks(2)
        .map(|p| format!("{}={}", p[0], p[1]))
        .collect();
    let eq: Vec<&str> = eq.iter().map(String::as_str).collect();
    assert_eq!(parse(&eq).expect("= form").listen, cfg.listen);
    for (i, name) in FLAGS.iter().step_by(2).enumerate() {
        if *name == "--socket" {
            continue;
        }
        let mut without = FLAGS.to_vec();
        without.drain(i * 2..i * 2 + 2);
        assert_eq!(
            parse(&without).expect_err(name),
            format!("{name} is required")
        );
    }
    let twice = [&FLAGS[..], &["--socket", "/other"]].concat();
    assert_eq!(parse(&twice).expect_err("twice"), "--socket is given twice");
    assert_eq!(
        parse(&["serve"]).expect_err("unknown"),
        "unknown argument serve"
    );
    assert_eq!(
        parse(&["--state-dir"]).expect_err("no value"),
        "--state-dir needs a value"
    );
    let mut bad_uid = FLAGS.to_vec();
    bad_uid[7] = "root";
    assert_eq!(
        parse(&bad_uid).expect_err("uid"),
        "--proxy-uid must be a number"
    );
    bad_uid[7] = "-1";
    assert!(parse(&bad_uid).is_err());
}

fn without_socket() -> Vec<&'static str> {
    let mut list = FLAGS.to_vec();
    list.drain(4..6);
    list
}

fn activation(pid: &str, fds: Option<&str>) -> Activation {
    Activation {
        pid: Some(pid.to_owned()),
        fds: fds.map(str::to_owned),
    }
}

/// Catches the socket coming from the wrong place or from two places: a descriptor passed to
/// this process (LISTEN_PID is our pid, LISTEN_FDS is 1) is fd 3 and needs no `--socket`;
/// `--socket` alone binds; both, or neither, is an error; a LISTEN_PID of another process,
/// a LISTEN_FDS other than 1 or none, and an unparsable pid are refused, with `--socket`
/// given or not (a stale environment must not pick the socket). Unset is not activation.
#[test]
fn the_socket_is_a_flag_or_an_activated_descriptor_and_never_both() {
    let me = std::process::id().to_string();
    let flags = without_socket();
    let cfg = parse_flags(&args(&flags), &activation(&me, Some("1"))).expect("activated");
    assert_eq!(cfg.listen, Listen::Activated(3));
    let both = parse_flags(&args(&FLAGS), &activation(&me, Some("1"))).expect_err("both");
    assert!(both.contains("use one"), "{both}");
    let neither = parse_flags(&args(&flags), &Activation::default()).expect_err("neither");
    assert!(
        neither.starts_with("--socket is required unless"),
        "{neither}"
    );
    assert!(neither.contains("LISTEN_FDS"), "{neither}");
    let other = (std::process::id() + 1).to_string();
    for (pid, fds, why) in [
        (other.as_str(), Some("1"), "another process"),
        ("abc", Some("1"), "another process"),
        (me.as_str(), Some("2"), "exactly one socket"),
        (me.as_str(), Some("0"), "exactly one socket"),
        (me.as_str(), None, "LISTEN_FDS is not"),
    ] {
        for list in [&flags[..], &FLAGS[..]] {
            let e = parse_flags(&args(list), &activation(pid, fds)).expect_err(why);
            assert!(e.contains(why), "{pid} {fds:?}: {e}");
        }
    }
    // LISTEN_FDS alone, without a LISTEN_PID, passes nothing.
    let stray = Activation {
        pid: None,
        fds: Some("1".to_owned()),
    };
    assert_eq!(
        parse_flags(&args(&FLAGS), &stray).expect("bind").listen,
        Listen::Bind("/sock".into())
    );
}

/// Asserts that `--operator-proxy-auth-file`, the flag of the secret a frontend behind the
/// proxy asked for, is an unknown argument now: the roll sends no such secret.
#[test]
fn the_operator_proxy_auth_file_flag_is_gone() {
    let with = [&FLAGS[..], &["--operator-proxy-auth-file", "/p"]].concat();
    assert_eq!(
        parse(&with).expect_err("gone"),
        "unknown argument --operator-proxy-auth-file"
    );
}

/// Catches the two ssh timeouts being required, read from the wrong flag, or taking a
/// non-number: they default to 10 s to connect and 900 s per call, and each flag sets only its
/// own.
#[test]
fn the_ssh_timeouts_are_optional_flags_in_seconds() {
    let cfg = parse(&FLAGS).expect("flags");
    assert_eq!(
        (
            cfg.ssh_connect_timeout.as_secs(),
            cfg.ssh_command_timeout.as_secs()
        ),
        (10, 900)
    );
    let with = [&FLAGS[..], &["--ssh-connect-timeout-secs", "3"]].concat();
    let cfg = parse(&with).expect("connect");
    assert_eq!(
        (
            cfg.ssh_connect_timeout.as_secs(),
            cfg.ssh_command_timeout.as_secs()
        ),
        (3, 900)
    );
    let with = [&FLAGS[..], &["--ssh-command-timeout-secs=60"]].concat();
    let cfg = parse(&with).expect("command");
    assert_eq!(
        (
            cfg.ssh_connect_timeout.as_secs(),
            cfg.ssh_command_timeout.as_secs()
        ),
        (10, 60)
    );
    for flag in ["--ssh-connect-timeout-secs", "--ssh-command-timeout-secs"] {
        for bad in ["soon", "-1", "1.5", ""] {
            let with = [&FLAGS[..], &[flag, bad]].concat();
            assert_eq!(
                parse(&with).expect_err(bad),
                format!("{flag} must be a number of seconds")
            );
        }
    }
}

/// Catches a drain deadline that is not a flag, that defaults to something else than 600 s,
/// that takes a non-number, or that may be zero (a drain that waits for nothing is no
/// drain): the flag sets only the deadline.
#[test]
fn the_drain_deadline_is_an_optional_flag_of_at_least_a_second() {
    assert_eq!(parse(&FLAGS).expect("flags").drain_deadline.as_secs(), 600);
    let with = [&FLAGS[..], &["--drain-deadline-secs", "90"]].concat();
    let cfg = parse(&with).expect("deadline");
    assert_eq!(
        (
            cfg.drain_deadline.as_secs(),
            cfg.ssh_command_timeout.as_secs()
        ),
        (90, 900)
    );
    for bad in ["soon", "-1", "1.5", ""] {
        let with = [&FLAGS[..], &["--drain-deadline-secs", bad]].concat();
        assert_eq!(
            parse(&with).expect_err(bad),
            "--drain-deadline-secs must be a number of seconds"
        );
    }
    let zero = [&FLAGS[..], &["--drain-deadline-secs", "0"]].concat();
    assert_eq!(
        parse(&zero).expect_err("zero"),
        "--drain-deadline-secs must be at least 1"
    );
}

/// Catches the two bounds of a Mac's apply not being flags: both are optional, default to a
/// 5 s poll and a 600 s deadline, take a number of seconds, and may not be zero (a poll with
/// no interval spins; a deadline of nothing fails every apply). Each sets only itself.
#[test]
fn the_apply_poll_and_deadline_are_optional_flags_of_at_least_a_second() {
    let cfg = parse(&FLAGS).expect("flags");
    assert_eq!(
        (cfg.apply_poll.as_secs(), cfg.apply_deadline.as_secs()),
        (5, 600)
    );
    let with = [&FLAGS[..], &["--apply-poll-secs", "2"]].concat();
    let cfg = parse(&with).expect("poll");
    assert_eq!(
        (cfg.apply_poll.as_secs(), cfg.apply_deadline.as_secs()),
        (2, 600)
    );
    let with = [&FLAGS[..], &["--apply-deadline-secs=90"]].concat();
    let cfg = parse(&with).expect("deadline");
    assert_eq!(
        (cfg.apply_poll.as_secs(), cfg.apply_deadline.as_secs()),
        (5, 90)
    );
    for flag in ["--apply-poll-secs", "--apply-deadline-secs"] {
        for bad in ["soon", "-1", "1.5", ""] {
            let with = [&FLAGS[..], &[flag, bad]].concat();
            assert_eq!(
                parse(&with).expect_err(bad),
                format!("{flag} must be a number of seconds")
            );
        }
        let zero = [&FLAGS[..], &[flag, "0"]].concat();
        assert_eq!(
            parse(&zero).expect_err("zero"),
            format!("{flag} must be at least 1")
        );
    }
}

/// Catches a flag that is parsed and never reaches the roll: the drain deadline and the apply
/// poll and deadline are the timing's, and the rest of the timing is the default.
#[test]
fn the_flags_set_the_timing_of_the_roll() {
    let with = [
        &FLAGS[..],
        &[
            "--drain-deadline-secs=7",
            "--apply-poll-secs=3",
            "--apply-deadline-secs=11",
        ],
    ]
    .concat();
    let t = crate::daemon::timing(&parse(&with).expect("flags"));
    assert_eq!(
        (
            t.drain_deadline.as_secs(),
            t.apply_poll.as_secs(),
            t.apply_deadline.as_secs()
        ),
        (7, 3, 11)
    );
    assert_eq!(t.poll, Timing::default().poll);
}

/// Catches the settings of the installer reaching it unchecked: a key path that is no file, a
/// user or channel the host would not take and a zero timeout each stop the ports from being
/// built, with the reason, and the key's contents are in no message.
#[test]
fn the_ssh_settings_are_checked_when_the_ports_are_built() {
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let good = config("sshcfg", "st1 st1.farm.example storage KEY\n", &api);
    assert!(build_ports(&good).is_ok());

    let mut cfg = good.clone();
    cfg.ssh_key_file = cfg.ssh_key_file.with_file_name("missing");
    let e = build_ports(&cfg).err().expect("missing key");
    assert!(
        e.starts_with("--ssh-key-file ") && e.ends_with("is not a file"),
        "{e}"
    );
    cfg.ssh_key_file = cfg.state_dir.clone();
    assert!(
        build_ports(&cfg)
            .err()
            .expect("directory")
            .contains("is not a file")
    );

    for (user, why) in [("Root", "ssh user"), ("", "ssh user"), ("-x", "ssh user")] {
        let mut cfg = good.clone();
        cfg.ssh_user = user.to_owned();
        let e = build_ports(&cfg).err().expect(user);
        assert!(e.contains(why), "{user}: {e}");
    }
    let mut cfg = good.clone();
    cfg.channel = "Stable".to_owned();
    assert!(
        build_ports(&cfg)
            .err()
            .expect("channel")
            .contains("channel")
    );
    for zero in [0, 1] {
        let mut cfg = good.clone();
        if zero == 0 {
            cfg.ssh_connect_timeout = std::time::Duration::ZERO;
        } else {
            cfg.ssh_command_timeout = std::time::Duration::ZERO;
        }
        let e = build_ports(&cfg).err().expect("zero");
        assert!(e.contains("more than zero"), "{e}");
    }
}

/// Catches the operator token being sent anywhere but loopback in clear: a non-loopback IP, a
/// name (no resolver is used), `https`, no scheme and a path or junk are refused; both
/// loopback families are taken.
#[test]
fn the_operator_api_must_be_a_loopback_address() {
    for ok in [
        "http://127.0.0.1:8080",
        "http://127.0.0.1:8080/",
        "http://[::1]:9",
    ] {
        assert!(parse_operator_url(ok).is_ok(), "{ok}");
    }
    assert_eq!(
        parse_operator_url("http://[::ffff:127.0.0.1]:9").map(|a| a.port()),
        Ok(9),
        "a mapped loopback is loopback"
    );
    for (bad, why) in [
        ("http://192.0.2.5:8080", "loopback"),
        ("http://0.0.0.0:8080", "loopback"),
        ("http://[2001:db8::1]:80", "loopback"),
        ("http://localhost:8080", "IP address"),
        ("http://example.com:80", "IP address"),
        ("http://127.0.0.1", "IP address"),
        ("http://127.0.0.1:8080/v1", "IP address"),
        ("https://127.0.0.1:8080", "http://"),
        ("127.0.0.1:8080", "http://"),
    ] {
        let e = parse_operator_url(bad).expect_err(bad);
        assert!(e.contains(why), "{bad}: {e}");
    }
}

/// Asserts that `--proxy-admin`, the flag of the pool step this roll once had, is an unknown
/// argument now.
#[test]
fn the_proxy_admin_flag_is_gone() {
    let with = [&FLAGS[..], &["--proxy-admin", "http://127.0.0.1:18090"]].concat();
    assert_eq!(
        parse(&with).expect_err("gone"),
        "unknown argument --proxy-admin"
    );
}

/// A config over a scratch directory with `inventory` and a good token file.
fn config(name: &str, inventory: &str, api: &FakeApi) -> Config {
    let dir = scratch(name);
    let state = dir.join("state");
    std::fs::create_dir(&state).expect("state");
    Config {
        state_dir: state,
        inventory: file(&dir, "inventory", &inventory.replace("KEY", KEY), 0o644),
        inventory_owner: own_uid(),
        listen: Listen::Bind(dir.join("api.sock")),
        proxy_uid: own_uid(),
        operator_api: api.addr,
        token_file: file(&dir, "token", TOKEN, 0o600),
        ssh_key_file: file(&dir, "id_updater", KEY_TEXT, 0o600),
        ssh_user: "kbf".to_owned(),
        channel: "stable".to_owned(),
        ssh_connect_timeout: Duration::from_secs(5),
        ssh_command_timeout: Duration::from_secs(30),
        drain_deadline: Duration::from_secs(600),
        apply_poll: Duration::from_secs(5),
        apply_deadline: Duration::from_secs(600),
    }
}

/// What the private key file holds in these tests: it must reach no answer and no audit line.
const KEY_TEXT: &str = "PRIVATE-KEY-BYTES-do-not-leak";

/// An [`Exec`] that records each process it is asked to run, one line per call (the program
/// and its arguments), and answers as the host's forced command does.
#[derive(Default)]
struct FakeSsh {
    calls: Mutex<Vec<String>>,
}

impl Exec for FakeSsh {
    fn run(&self, spawn: &Spawn) -> Result<Exited, ExecError> {
        let args: Vec<String> = spawn
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        locked(&self.calls).push(format!("{} {}", spawn.program.display(), args.join(" ")));
        let stdout = if args.last().is_some_and(|c| c.starts_with("install ")) {
            "{\"result\":\"installed\",\"installed_version\":\"2.0-1\",\"unit_state\":\"finished\"}\n"
        } else {
            "{\"result\":\"query\",\"installed_version\":\"2.0-1\",\"running_commit\":null,\"in_progress\":false}\n"
        };
        Ok(Exited {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stdout_overflow: false,
            stderr: Vec::new(),
        })
    }
}

/// The timing of a test: a roll that cannot go on fails within a second.
fn quick() -> Timing {
    Timing {
        poll: Duration::from_millis(10),
        safe_bound: Duration::from_millis(300),
        install_bound: Duration::from_millis(300),
        query_bound: Duration::from_millis(300),
        healthy_storage: Duration::from_millis(300),
        healthy_daemon: Duration::from_millis(300),
        drain_deadline: Duration::from_millis(300),
        apply_poll: Duration::from_millis(10),
        apply_deadline: Duration::from_millis(300),
    }
}

/// The binary's ports with `ssh` standing in for the real program.
fn start_with_ssh(cfg: &Config, ssh: &Arc<FakeSsh>) -> Daemon {
    start_with(
        cfg,
        build_ports_with_exec(cfg, ssh.clone()).expect("ports"),
        quick(),
        Settings::new(cfg.proxy_uid),
        stderr_sink(),
    )
    .expect("start")
}

fn wait_failed(socket: &std::path::Path) -> Roll {
    let mut last = None;
    eventually(|| {
        let roll: Roll =
            serde_json::from_value(call(socket, "GET", "/v1/updater/rolls/roll-1", "").1)
                .expect("roll");
        let over = roll.state != RollState::Running;
        last = Some(roll);
        over
    });
    last.expect("roll")
}

fn post(socket: &std::path::Path) -> (u16, Value) {
    let body = json!({"package": "kbf", "version": "2.0-1", "idempotency_key": "run-1"});
    call(socket, "POST", "/v1/updater/rolls", &body.to_string())
}

/// Asserts that ssh is never run for a frontend: with the binary's own ports, an all-hosts roll
/// (frontend, storage, daemon) is accepted (202) and fails at the frontend, before its
/// installer, with the reason that says frontends are not rolled here. The recording `Exec` is never called (the recorded calls are empty), no later
/// host is asked about (the fake operator API sees no request at all) and the later hosts stay
/// pending.
#[test]
fn an_all_hosts_roll_never_runs_ssh_for_a_frontend() {
    let api = fake_api(vec![Some(http(
        200,
        "{\"safe\":true,\"reasons\":[],\"planned_down\":false}",
    ))]);
    let cfg = config(
        "unbuilt",
        "fe fe.farm.example frontend KEY\nst1 st1.farm.example storage KEY\n\
         dm1 dm1.farm.example daemon KEY\n",
        &api,
    );
    let ssh = Arc::new(FakeSsh::default());
    let daemon = start_with_ssh(&cfg, &ssh);
    let (status, body) = post(&sock(&cfg));
    assert_eq!((status, &body["state"]), (202, &json!("running")), "{body}");
    let roll = wait_failed(&sock(&cfg));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(
        (failure.alias.as_str(), failure.code),
        ("fe", ErrorCode::SwapAbandoned)
    );
    assert!(
        failure
            .detail
            .contains(crate::node_health::FRONTEND_REFUSAL),
        "{}",
        failure.detail
    );
    let phases: Vec<_> = roll
        .hosts
        .iter()
        .map(|h| format!("{:?}", h.phase))
        .collect();
    assert_eq!(phases, ["Failed", "Pending", "Pending"]);
    assert!(
        locked(&ssh.calls).is_empty(),
        "ssh was run: {:?}",
        locked(&ssh.calls)
    );
    assert!(
        api.requests().is_empty(),
        "no host was asked about: {:?}",
        api.requests()
    );
    daemon.stopper().stop();
    daemon.wait();
}

/// Catches the ssh installer not being the one wired into the roll for a node: a storage
/// host is checked with the token, installed through `ssh` (exactly one install call, as the
/// configured user, with the configured key file and channel, to the host's address, with
/// the version of the roll as the remote command), and the roll then goes on to ask for its
/// health: the version of the test names no commit, so the node health check abandons it,
/// which is a health failure and not an install one. The key's contents
/// are in no argument, no answer and no audit line.
#[test]
fn a_node_is_installed_over_ssh_with_the_configured_user_key_and_channel() {
    let api = fake_api(vec![Some(http(
        200,
        "{\"safe\":true,\"reasons\":[],\"planned_down\":false}",
    ))]);
    let cfg = config("viassh", "st1 st1.farm.example:2222 storage KEY\n", &api);
    let ssh = Arc::new(FakeSsh::default());
    let audit = Lines::default();
    let daemon = start_with(
        &cfg,
        build_ports_with_exec(&cfg, ssh.clone()).expect("ports"),
        quick(),
        Settings::new(cfg.proxy_uid),
        audit.sink(),
    )
    .expect("start");
    assert_eq!(post(&sock(&cfg)).0, 202);
    let roll = wait_failed(&sock(&cfg));
    let failure = roll.failure.expect("failure");
    assert_eq!(
        (failure.alias.as_str(), failure.code),
        ("st1", ErrorCode::SwapAbandoned)
    );
    assert!(
        failure.detail.contains("names no commit"),
        "{}",
        failure.detail
    );
    let calls = locked(&ssh.calls).join("\n");
    let installs: Vec<&str> = calls
        .lines()
        .filter(|l| l.contains(" install kbf="))
        .collect();
    assert_eq!(installs.len(), 1, "{calls}");
    let key = cfg.ssh_key_file.display().to_string();
    for want in [
        format!("-i {key} "),
        "kbf@st1.farm.example".to_owned(),
        "-p 2222".to_owned(),
        "install kbf=2.0-1 from stable".to_owned(),
    ] {
        assert!(installs[0].contains(&want), "{want}: {}", installs[0]);
    }
    let seen = api.requests();
    assert!(
        seen[0].starts_with("GET /v1/members/st1/safe-to-stop "),
        "{seen:?}"
    );
    assert!(seen[0].contains(&format!("Bearer {TOKEN}")));
    let everything = format!(
        "{calls}{}{}",
        audit.raw(),
        call(&sock(&cfg), "GET", "/v1/updater/rolls", "").1
    );
    assert!(!everything.contains(KEY_TEXT) && !everything.contains(TOKEN));
    daemon.stopper().stop();
    daemon.wait();
}

/// Catches a role that is not frontend, storage, daemon or mac-daemon being accepted: the
/// inventory refuses the line (`mac` alone is not a role), so the updater does not start and
/// no ssh runs.
#[test]
fn an_unknown_role_is_refused_by_the_inventory() {
    let api = fake_api(vec![Some(http(200, "{}"))]);
    for role in ["mac", "macos", "homebrew"] {
        let cfg = config(
            "mac",
            &format!("st1 st1.farm.example storage KEY\nm1 m1.farm.example {role} KEY\n"),
            &api,
        );
        let e = start(&cfg).err().expect(role);
        assert!(
            e.starts_with("inventory ")
                && e.contains("is not frontend, storage, daemon or mac-daemon"),
            "{e}"
        );
        assert!(!sock(&cfg).exists());
    }
}

/// Asserts that an inventory that still lists a frontend as a pool member (`instance=`,
/// `base=`) is refused at start-up with the line named and nothing bound.
#[test]
fn an_inventory_with_pool_members_is_refused_at_start() {
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let members = "fe-a fe.farm.example frontend KEY instance=a base=18000\n";
    let cfg = config("members", members, &api);
    let e = start(&cfg).err().expect("refused");
    assert!(e.starts_with("inventory "), "{e}");
    assert!(e.contains("line 1"), "{e}");
    assert!(e.contains("no longer rolls frontends"), "{e}");
    assert!(!sock(&cfg).exists());
}

/// Catches start-up that goes on over something wrong: an inventory that is not owned by the
/// expected user or is missing, a state directory that does not exist or is locked by another
/// updater, a corrupt record, a socket someone listens at, and a token file that is no good.
/// Each is an error naming what failed, and the socket is not left bound.
#[test]
fn start_up_refuses_what_it_cannot_trust() {
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let good = "st1 st1.farm.example storage KEY\n";

    let mut cfg = config("bad1", good, &api);
    cfg.inventory_owner = own_uid() + 1;
    let e = start(&cfg).err().expect("owner");
    assert!(e.starts_with("inventory "), "{e}");
    cfg.inventory = cfg.inventory.with_file_name("missing");
    assert!(
        start(&cfg)
            .err()
            .expect("missing")
            .starts_with("inventory ")
    );

    let cfg = config("bad2", good, &api);
    let held = RollStore::open(&cfg.state_dir).expect("lock");
    let e = start(&cfg).err().expect("locked");
    assert!(e.starts_with("state dir "), "{e}");
    drop(held);
    std::fs::write(cfg.state_dir.join("roll.json"), "{ not json").expect("corrupt");
    let e = start(&cfg).err().expect("corrupt");
    assert!(e.starts_with("roll record in "), "{e}");
    assert!(
        !sock(&cfg).exists(),
        "nothing is bound when the record is bad"
    );

    let mut cfg = config("bad3", good, &api);
    let _live = UnixListener::bind(sock(&cfg)).expect("someone listens");
    let e = start(&cfg).err().expect("live socket");
    assert!(e.starts_with("socket ") && e.contains("listens"), "{e}");

    cfg.token_file = file(&scratch("bad4"), "token", "short", 0o600);
    let e = build_ports(&cfg).err().expect("token");
    assert!(e.contains("32 to 4096") && !e.contains("short\n"), "{e}");
    assert!(start(&cfg).is_err());
}

/// Catches a queued roll that cannot be driven stopping the worker or going unreported: the
/// loop audits it and goes on to the next.
#[test]
fn the_driver_loop_audits_what_it_cannot_drive_and_goes_on() {
    let cfg = config(
        "loop",
        "st1 st1.farm.example storage KEY\n",
        &fake_api(vec![Some(http(200, "{}"))]),
    );
    let log = Log::default();
    let ports = Ports {
        safe: Box::new(FakeSafeToStop::new(&log)),
        installer: Box::new(FakeInstaller::new(&log)),
        drain: Box::new(crate::fakes::FakeDrain::new(&log)),
        health: Box::new(FakeHealth::new(&log)),
        clock: Box::new(FakeClock::new()),
    };
    let updater = Updater::open(
        crate::inventory::Inventory::load(&cfg.inventory, own_uid()).expect("inventory"),
        RollStore::open(&cfg.state_dir).expect("store"),
        ports,
        Timing::default(),
    )
    .expect("open");
    let req = crate::api::RollRequest::from_json(
        json!({"package": "kbf", "version": "2", "idempotency_key": "k"})
            .to_string()
            .as_bytes(),
    )
    .expect("request");
    let id = updater.start(&req).expect("start").roll.id;
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send("roll-99".to_owned()).expect("send");
    tx.send(id).expect("send");
    drop(tx);
    let (audit, recent) = (Lines::default(), Recent::default());
    drive_loop(&updater, rx, &recent, &audit.sink());
    let lines = audit.all();
    assert_eq!(lines[0]["event"], "roll_not_driven");
    assert_eq!(lines[0]["roll_id"], "roll-99");
    assert_eq!(
        (lines[1]["event"].as_str(), lines[1]["state"].as_str()),
        (Some("roll_finished"), Some("done"))
    );
    assert_eq!(recent.list_except(None).len(), 1);
    assert_eq!(
        log.events()
            .iter()
            .filter(|e| matches!(e, Event::Install(..)))
            .count(),
        1
    );
}

/// Catches the process entry points: `--version`, a usage error for no flags, an unknown
/// argument and a missing flag (exit 2 with the usage), a start that fails (exit 1 with the
/// reason, and nothing bound), and a run that serves, answers, stops and exits 0 with its
/// socket removed.
#[test]
fn the_process_entry_points() {
    let (code, out) = run_with(&args(&["kbf-updater", "--version"]), |_| {
        panic!("not started")
    });
    assert_eq!((code, out.starts_with("kbf-updater 0.")), (0, true));
    for bad in [
        &["kbf-updater"][..],
        &["kbf-updater", "serve"],
        &["kbf-updater", "--socket", "/x"],
    ] {
        let (code, out) = run_with(&args(bad), |_| panic!("not started"));
        assert_eq!(code, 2, "{bad:?}");
        assert!(out.contains("usage: kbf-updater --version"), "{out}");
    }
    // Valid flags, but the inventory does not exist: exit 1 and the reason.
    let api = fake_api(vec![Some(http(200, "{}"))]);
    let cfg = config("entry", "st1 st1.farm.example storage KEY\n", &api);
    let socket_path = sock(&cfg);
    let a = [
        "kbf-updater",
        "--state-dir",
        cfg.state_dir.to_str().expect("utf8"),
        "--inventory",
        "/nonexistent/inventory",
        "--socket",
        socket_path.to_str().expect("utf8"),
        "--proxy-uid",
        "1",
        "--operator-api",
        "http://127.0.0.1:1",
        "--token-file",
        cfg.token_file.to_str().expect("utf8"),
        "--ssh-key-file",
        cfg.ssh_key_file.to_str().expect("utf8"),
        "--ssh-user",
        "kbf",
        "--channel",
        "stable",
    ];
    let (code, out) = run_with(&args(&a), |_| panic!("not started"));
    assert_eq!(code, 1);
    assert!(
        out.starts_with("kbf-updater: inventory /nonexistent/inventory"),
        "{out}"
    );
    assert!(!sock(&cfg).exists());
    // A run that serves.
    let socket = sock(&cfg).clone();
    let (code, out) = run_config(&cfg, |stopper| {
        let socket = socket.clone();
        std::thread::spawn(move || {
            assert_eq!(call(&socket, "GET", "/v1/updater/rolls", "").0, 200);
            stopper.stop();
        });
    });
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(!sock(&cfg).exists(), "the socket is removed on exit");
    stderr_sink()("{\"event\":\"test\"}");
}
