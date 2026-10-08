//! The socket, the protocol and start-up, against this test binary as the daemon.

use std::net::Shutdown;
use std::os::unix::fs::MetadataExt as _;

use super::*;
use crate::apply::FakeApplier;
use crate::set::Pin;
use crate::signed::public_key;
use crate::testkit::{self, PLATFORM_SEED, ROOT_SEED};

fn me() -> DaemonPin {
    DaemonPin {
        uid: rustix::process::geteuid().as_raw(),
        path: std::env::current_exe().unwrap(),
    }
}

fn gid() -> u32 {
    rustix::process::getegid().as_raw()
}

fn config(dir: &Path) -> Config {
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    Config {
        root_key: public_key(&ROOT_SEED),
        pin: Pin {
            pool: "linux-x86".into(),
            platform: host_platform(),
        },
        state_dir,
        artifacts_dir: dir.join("artifacts"),
    }
}

fn updater(dir: &Path) -> Updater<FakeApplier> {
    Updater::open(config(dir), FakeApplier::default()).unwrap()
}

/// Sends one request over a connected stream and reads the reply.
fn exchange(mut s: UnixStream, body: &str) -> String {
    s.write_all(body.as_bytes()).unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    reply
}

/// Catches: the host's architecture misspelled against the capability keys (`arm64`,
/// not `aarch64`), so a correctly pinned node refuses to start.
#[test]
fn the_host_platform_uses_capability_spellings() {
    let host = host_platform();
    assert_eq!(host.os, "linux");
    assert!(
        ["x86_64", "arm64"].contains(&host.arch.as_str()),
        "{host:?}"
    );
    assert_eq!(capability_arch("aarch64"), "arm64");
    assert_eq!(capability_arch("x86_64"), "x86_64");
}

/// Catches: a root key file read with its trailing newline (no node could start), or a
/// missing or bad file accepted.
#[test]
fn the_root_key_file_holds_hex() {
    let dir = testkit::scratch("root-key");
    let path = dir.join("root.pub");
    fs::write(&path, format!("{}\n", hex::encode(public_key(&ROOT_SEED)))).unwrap();
    assert_eq!(read_root_key(&path), Ok(public_key(&ROOT_SEED)));
    fs::write(&path, "nothex").unwrap();
    assert!(read_root_key(&path).is_err());
    assert!(read_root_key(&dir.join("missing")).is_err());
}

/// Catches: dropping any start-up refusal: an old kernel, a pin for another platform,
/// a missing daemon binary, or a native daemon running.
#[test]
fn start_up_refuses_old_kernels_foreign_pins_and_native_daemons() {
    let dir = testkit::scratch("startup");
    let daemon = dir.join("kbf-daemon");
    fs::write(&daemon, b"daemon").unwrap();
    let procs = dir.join("proc");
    fs::create_dir_all(procs.join("42")).unwrap();
    fs::write(
        procs.join("42").join("cmdline"),
        b"kbf-daemon\0--driver\0container",
    )
    .unwrap();
    fs::write(procs.join("42").join("exe"), b"daemon").unwrap();
    let host = host_platform();
    let check = |release: &str, pinned: &Platform, daemon: &Path| {
        startup_checks(release, pinned, &host, &procs, daemon)
    };
    assert_eq!(check("6.8.0-45-generic", &host, &daemon), Ok(()));
    let old = check("6.4.0", &host, &daemon).unwrap_err();
    assert!(old.contains("6.5 or later"), "{old}");
    let mac = Platform {
        os: "macos".into(),
        arch: "arm64".into(),
    };
    assert!(
        check("6.8.0", &mac, &daemon)
            .unwrap_err()
            .contains("pinned platform")
    );
    assert!(check("6.8.0", &host, &dir.join("missing")).is_err());
    fs::write(
        procs.join("42").join("cmdline"),
        b"kbf-daemon\0--driver\0native",
    )
    .unwrap();
    let native = check("6.8.0", &host, &daemon).unwrap_err();
    assert!(
        native.contains("pid 42") && native.contains("--driver native"),
        "{native}"
    );
}

/// Catches: the socket made 0666 or its directory open to all (any local process could
/// connect), the group not set, or a stale socket blocking a restart.
#[test]
fn the_socket_is_group_only() {
    let short = testkit::short_dir("bind");
    let dir = &short.path;
    let socket = dir.join("run").join("s");
    let first = bind(&socket, gid()).unwrap();
    drop(first);
    let _again = bind(&socket, gid()).unwrap();
    let dir_meta = fs::metadata(socket.parent().unwrap()).unwrap();
    let sock_meta = fs::metadata(&socket).unwrap();
    assert_eq!(dir_meta.mode() & 0o7777, 0o750);
    assert_eq!(sock_meta.mode() & 0o7777, 0o660);
    assert_eq!((dir_meta.gid(), sock_meta.gid()), (gid(), gid()));
    assert!(bind(Path::new("/"), gid()).is_err());
    let busy = dir.join("busy");
    fs::create_dir_all(busy.join("s")).unwrap();
    assert!(
        bind(&busy.join("s"), gid()).is_err(),
        "a directory in the socket's place"
    );
}

/// Catches: the socket's directory taken when it is a symbolic link (the group and mode
/// would land on whatever it points at, and the socket in a directory someone else
/// controls), or when another user owns it (S4.3: the updater's own, so root's).
#[test]
fn the_socket_directory_is_the_updaters_own() {
    let short = testkit::short_dir("bind-owner");
    let dir = &short.path;
    let real = dir.join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
    assert!(bind(&dir.join("link").join("s"), gid()).is_err());
    assert!(!real.join("s").exists());
    assert_eq!(fs::metadata(&real).unwrap().mode() & 0o7777, 0o755);
    let me = rustix::process::geteuid().as_raw();
    let err = bind_owned_by(&real.join("s"), gid(), me + 1).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert!(!real.join("s").exists());
    assert_eq!(fs::metadata(&real).unwrap().mode() & 0o7777, 0o755);
    bind_owned_by(&real.join("s"), gid(), me).unwrap();
    assert_eq!(fs::metadata(&real).unwrap().mode() & 0o7777, 0o750);
}

/// Catches: an oversized or malformed request reaching the verbs, a verb outside the
/// three accepted, or replies in another shape.
#[test]
fn requests_are_one_of_three_verbs() {
    let dir = testkit::scratch("respond");
    let mut u = updater(&dir);
    let status = respond(br#"{"verb":"status"}"#, &mut u, 0);
    assert_eq!(status["ok"]["pool"], "linux-x86");
    assert_eq!(status["ok"]["floor"], 0);
    for bad in [
        &br#"{"verb":"reboot"}"#[..],
        br#"{"verb":"rollback"}"#,
        br#"{"verb":"status","x":1}"#,
        b"{",
    ] {
        let r = respond(bad, &mut u, 0);
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .starts_with("malformed: request"),
            "{r}"
        );
    }
    let huge = vec![b' '; usize::try_from(MAX_REQUEST).unwrap() + 1];
    assert!(
        respond(&huge, &mut u, 0)["error"]
            .as_str()
            .unwrap()
            .contains("bytes")
    );
    // The updater here is pinned to this machine's platform; so is the set.
    let mut set = testkit::set(5);
    set.platform = host_platform();
    let set = testkit::seal_set(&set, &PLATFORM_SEED);
    let st = testkit::seal_statement(&testkit::statement(1, &[]));
    let stage = json!({ "verb": "stage", "set": set });
    assert_eq!(
        respond(stage.to_string().as_bytes(), &mut u, 0),
        json!({ "error": "no key statement" })
    );
    let apply = json!({ "verb": "apply", "set": set, "statement": st });
    let r = respond(apply.to_string().as_bytes(), &mut u, 0);
    assert!(
        r["error"].as_str().unwrap().contains("is not staged"),
        "{r}"
    );
    let artifacts = dir.join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    for content in ["d", "u"] {
        fs::write(artifacts.join(testkit::sha(content)), content).unwrap();
    }
    let stage = json!({ "verb": "stage", "set": set, "statement": st });
    let r = respond(stage.to_string().as_bytes(), &mut u, 0);
    assert_eq!(r, json!({ "ok": "staged" }));
    let r = respond(apply.to_string().as_bytes(), &mut u, 0);
    assert_eq!(r, json!({ "ok": { "applied": { "reboot": false } } }));
    let r = respond(apply.to_string().as_bytes(), &mut u, 0);
    assert_eq!(r, json!({ "ok": "already_installed" }));
}

/// Catches: a reply sent to a refused caller (or its request served), or the genuine
/// daemon's request not answered.
#[test]
fn only_the_daemon_gets_a_reply() {
    let dir = testkit::scratch("handle");
    let mut u = updater(&dir);
    let (ours, theirs) = UnixStream::pair().unwrap();
    let wrong = DaemonPin {
        uid: me().uid + 1,
        ..me()
    };
    theirs.shutdown(Shutdown::Write).unwrap();
    handle(ours, &mut u, &wrong, 0).unwrap();
    assert_eq!(exchange(theirs, ""), "");
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    theirs.write_all(br#"{"verb":"status"}"#).unwrap();
    theirs.shutdown(Shutdown::Write).unwrap();
    handle(ours, &mut u, &me(), 0).unwrap();
    let mut reply = String::new();
    theirs.read_to_string(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"]["pool"], "linux-x86");
    // A caller that hangs up before the reply is an error for the log, not a crash.
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    theirs.write_all(br#"{"verb":"status"}"#).unwrap();
    drop(theirs);
    assert!(handle(ours, &mut u, &me(), 0).is_err());
}

/// Catches: `run` not serving after its start-up checks, or a failed connection ending
/// the service.
#[test]
fn run_serves_the_socket() {
    let short = testkit::short_dir("run");
    let dir = &short.path;
    let socket = dir.join("r").join("s");
    let settings = Settings {
        socket: socket.clone(),
        socket_gid: gid(),
        daemon: me(),
        config: config(dir),
        proc_root: dir.join("no-proc"),
    };
    std::thread::spawn(move || run(settings, FakeApplier::default()));
    let connect = || {
        for _ in 0..500 {
            if let Ok(s) = UnixStream::connect(&socket) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the updater never listened");
    };
    let mut hang_up = connect();
    hang_up.write_all(br#"{"verb":"status"}"#).unwrap();
    drop(hang_up);
    let reply: Value = serde_json::from_str(&exchange(connect(), r#"{"verb":"status"}"#)).unwrap();
    assert_eq!(reply["ok"]["installed"], Value::Null);
}

/// Catches: `run` serving after a failed start-up step.
#[test]
fn run_stops_at_a_failed_start_up_step() {
    let dir = testkit::scratch("run-fail");
    let settings = Settings {
        socket: PathBuf::from("/"),
        socket_gid: gid(),
        daemon: me(),
        config: config(&dir),
        proc_root: dir.join("no-proc"),
    };
    let mut foreign = settings.clone();
    foreign.config.pin.platform.os = "macos".into();
    assert!(
        run(foreign, FakeApplier::default())
            .unwrap_err()
            .contains("pinned platform")
    );
    let bad_state = settings.clone();
    fs::create_dir_all(bad_state.config.state_dir.join("state.json")).unwrap();
    assert!(
        run(bad_state, FakeApplier::default())
            .unwrap_err()
            .starts_with("state:")
    );
    fs::remove_dir(settings.config.state_dir.join("state.json")).unwrap();
    assert!(
        run(settings, FakeApplier::default())
            .unwrap_err()
            .starts_with("/:")
    );
}

/// Catches: the clock read as zero (every set would look unexpired forever).
#[test]
fn now_is_the_wall_clock() {
    assert!(unix_now() > 1_790_000_000);
}
