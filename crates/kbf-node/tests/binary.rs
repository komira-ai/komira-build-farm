//! The `kbf-daemon` binary as a node runs it: its flags, each driver brought up far
//! enough to detect the node and start the session loop, and a clean exit on SIGTERM;
//! and, against a fake front ([`front`]), what a restarted daemon ends before `Hello`.

mod front;

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-daemon");

/// Catches: a binary that fails to start, exits non-zero on `--version`, or reports a
/// name or version other than its own package's and the commit it was built from
/// (issue #170).
#[test]
fn prints_name_and_version_and_exits_zero() {
    let out = Command::new(BIN).arg("--version").output().expect("spawn");
    assert!(
        out.status.success(),
        "kbf-daemon exited with {}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let prefix = format!("kbf-daemon {}+", env!("CARGO_PKG_VERSION"));
    let commit = stdout
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("{stdout:?} is not {prefix}<commit>"));
    assert_eq!(commit.len(), 12, "{stdout:?}");
    assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()), "{stdout:?}");
}

/// Catches: a binary that runs without its required flags instead of refusing them,
/// or that offers a driver other than the three.
#[test]
fn refuses_to_run_without_its_flags() {
    let out = Command::new(BIN).output().expect("spawn");
    assert_eq!(out.status.code(), Some(2), "clap's usage error exit code");
    let stderr = String::from_utf8(out.stderr).expect("stderr is UTF-8");
    for flag in [
        "--server",
        "--ca-cert",
        "--cert",
        "--key",
        "--node-id",
        "--driver",
    ] {
        assert!(
            stderr.contains(flag),
            "{flag} not named as required: {stderr}"
        );
    }
    let help = Command::new(BIN).arg("--help").output().expect("spawn");
    let help = String::from_utf8(help.stdout).expect("UTF-8");
    for driver in ["fake:", "container:", "native:"] {
        assert!(help.contains(driver), "{driver} not offered: {help}");
    }
}

/// A scratch directory for one test, with a CA and a client certificate in it.
fn tls(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-node")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(Vec::new())
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("node.pem"), cert.pem()).expect("write");
    std::fs::write(dir.join("node.key"), key.serialize_pem()).expect("write");
    // The fake front's own certificate ([`front::Front`]), for `localhost`.
    let server_key = KeyPair::generate().expect("key");
    let server = CertificateParams::new(vec!["localhost".to_owned()])
        .expect("params")
        .signed_by(&server_key, &ca)
        .expect("sign");
    std::fs::write(dir.join("server.pem"), server.pem()).expect("write");
    std::fs::write(dir.join("server.key"), server_key.serialize_pem()).expect("write");
    dir
}

/// The flags every run here shares: a front on a port nothing listens on.
fn base(dir: &Path) -> Vec<String> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    vec![
        format!("--server=https://127.0.0.1:{port}"),
        format!("--ca-cert={}", dir.join("ca.pem").display()),
        format!("--cert={}", dir.join("node.pem").display()),
        format!("--key={}", dir.join("node.key").display()),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--label=pool=darwin-sized".to_owned(),
    ]
}

/// Starts the daemon with `extra` flags, its stderr a pipe, waits until its session
/// loop has failed to connect once (so detection, the driver and the TLS files all
/// worked), then sends SIGTERM and returns its exit status. Checks the log on the way
/// (issue #170): no terminal escape codes, since stderr is not a terminal, and the
/// failed connection's cause under tonic's bare `transport error`.
fn runs_until_sigterm(name: &str, extra: &[String]) -> std::process::ExitStatus {
    runs_until_sigterm_with_path(name, extra, None)
}

/// [`runs_until_sigterm`], with `path` searched first for programs (`podman`).
fn runs_until_sigterm_with_path(
    name: &str,
    extra: &[String],
    path: Option<&Path>,
) -> std::process::ExitStatus {
    let dir = tls(name);
    let started = Instant::now();
    let mut command = Command::new(BIN);
    if let Some(path) = path {
        let system = std::env::var_os("PATH").unwrap_or_default();
        let mut joined = path.as_os_str().to_owned();
        joined.push(":");
        joined.push(system);
        command.env("PATH", joined);
    }
    let mut child = command
        .args(base(&dir))
        .args(extra)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // Read on another thread to the end, so the daemon never writes to a closed pipe.
    let stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let (lines, seen) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut log = Vec::new();
    while !log.iter().any(|l: &String| l.contains("session ended")) {
        let left = deadline.saturating_duration_since(Instant::now());
        match seen.recv_timeout(left) {
            Ok(line) => log.push(line),
            Err(e) => panic!("no session attempt ({e}): {log:#?}"),
        }
    }
    // How long the daemon took to try its first session (the native driver surveys its
    // Xcodes before, and logs how long that took), for the CI log: written to stderr,
    // which tests do not capture.
    let survey = log.iter().find(|l| l.contains("Xcodes")).map_or("", |l| l.as_str());
    let _ = writeln!(
        std::io::stderr(),
        "binary.rs: the {name} daemon tried its first session {:.1?} after it started; {survey}",
        started.elapsed()
    );
    let pid = i32::try_from(child.id()).expect("pid");
    // SAFETY: kill(2) on the child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = child.wait().expect("wait");
    reader.join().expect("reader");
    let rest: Vec<String> = seen.try_iter().collect();
    assert!(
        rest.iter().any(|l| l.contains("SIGTERM: shutting down")),
        "{rest:#?}"
    );
    let all: Vec<&String> = log.iter().chain(&rest).collect();
    assert!(
        all.iter().all(|l| !l.contains('\u{1b}')),
        "escape codes in a log that is not a terminal: {all:#?}"
    );
    let ended = log
        .iter()
        .find(|l| l.contains("session ended"))
        .expect("seen");
    assert!(
        ended.contains("connect: transport error: ") && ended.contains("refused"),
        "no cause: {ended}"
    );
    status
}

/// Catches: a driver that cannot be brought up from the command line (a missing
/// flag, a scratch directory not made, a node that cannot be detected), and a daemon
/// that does not exit cleanly on SIGTERM.
#[test]
fn each_driver_starts_and_stops_on_sigterm() {
    let status = runs_until_sigterm("fake", &["--driver=fake".to_owned()]);
    assert!(status.success(), "{status}");
    let scratch = tls("native-scratch").join("leases");
    let native = [
        "--driver=native".to_owned(),
        "--cas=http://127.0.0.1:1".to_owned(),
        format!("--scratch={}", scratch.display()),
    ];
    let status = runs_until_sigterm("native", &native);
    assert!(status.success(), "{status}");
    assert!(
        scratch.is_dir(),
        "the native driver makes its scratch directory"
    );
    if cfg!(target_os = "linux") {
        // A full range of subordinate ids for this user, whatever the host's files say.
        let ids = id_files("container-ids", "65536");
        let container = [
            "--driver=container".to_owned(),
            "--cas=http://127.0.0.1:1".to_owned(),
            format!("--scratch={}", scratch.display()),
            "--cgroup-parent=/kbf.slice/actions".to_owned(),
            format!("--id-files={}", ids.display()),
        ];
        // A `podman` that lists no container and logs its arguments: the start-up
        // sweep asks it for this node's leftovers before the daemon connects.
        // A symlink, not a written script: exec'ing a file this process just wrote can
        // fail with ETXTBSY while another test thread forks.
        let bin = tls("container-bin");
        let log = bin.join("podman.log");
        let _ = std::fs::remove_file(bin.join("podman"));
        let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/podman-stub.sh");
        std::os::unix::fs::symlink(stub, bin.join("podman")).expect("link podman");
        let status = runs_until_sigterm_with_path("container", &container, Some(&bin));
        assert!(status.success(), "{status}");
        let asked = read(&log);
        assert!(
            asked
                .lines()
                .any(|a| a == "--filter=label=kbf.owner=node-1"),
            "the sweep looks for this node's containers: {asked}"
        );
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

/// A directory for `--id-files`: a passwd naming this test's user `kbf`, and a
/// subuid and subgid giving `kbf` `count` subordinate ids (none when empty).
fn id_files(name: &str, count: &str) -> PathBuf {
    use std::os::unix::fs::MetadataExt;

    let dir = tls(name);
    let passwd = dir.join("passwd");
    std::fs::write(&passwd, "").expect("write");
    let uid = std::fs::metadata(&passwd).expect("stat").uid();
    std::fs::write(&passwd, format!("kbf:x:{uid}:{uid}::/:/bin/sh\n")).expect("write");
    let range = if count.is_empty() {
        String::new()
    } else {
        format!("kbf:100000:{count}\n")
    };
    for file in ["subuid", "subgid"] {
        std::fs::write(dir.join(file), &range).expect("write");
    }
    dir
}

/// Catches: a driver started without what it needs, or a configuration error that
/// does not stop the daemon with a message and a non-zero exit.
#[test]
fn a_driver_missing_its_flags_refuses_to_start() {
    let dir = tls("refused");
    let scratch = format!("--scratch={}", dir.join("leases").display());
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec!["--driver=native".into(), scratch.clone()],
            "--cas is required",
        ),
        (
            vec!["--driver=native".into(), "--cas=http://127.0.0.1:1".into()],
            "--scratch is required",
        ),
        (
            vec![
                "--driver=container".into(),
                "--cas=http://127.0.0.1:1".into(),
                scratch.clone(),
            ],
            if cfg!(target_os = "linux") {
                "--cgroup-parent is required"
            } else {
                "runs on Linux only"
            },
        ),
    ];
    for (extra, says) in cases {
        let out = Command::new(BIN)
            .args(base(&dir))
            .args(&extra)
            .output()
            .expect("spawn");
        assert_eq!(out.status.code(), Some(1), "{extra:?}");
        let stderr = String::from_utf8(out.stderr).expect("UTF-8");
        assert!(stderr.contains(says), "{extra:?}: {stderr}");
    }
    // TLS files that are not there stop the daemon before it connects.
    let mut flags = base(&dir);
    flags[2] = format!("--cert={}", dir.join("absent.pem").display());
    let out = Command::new(BIN)
        .args(&flags)
        .arg("--driver=fake")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("absent.pem"));
}

/// Catches the container driver starting on a node whose daemon user has no
/// subordinate ids, or too few (every lease would fail in `podman create`): the
/// "drop the startup check" mutant. The daemon exits non-zero naming the file and
/// the user, before it reaches the front.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_without_subordinate_ids_refuses_to_start() {
    for (name, count, says) in [
        (
            "no-ids",
            "",
            "subuid has no range for the daemon's user kbf",
        ),
        ("few-ids", "65535", "subuid gives the daemon's user kbf"),
    ] {
        let ids = id_files(name, count);
        let log = ids.join("stderr");
        let mut child = Command::new(BIN)
            .args(base(&ids))
            .args([
                "--driver=container".to_owned(),
                "--cas=http://127.0.0.1:1".to_owned(),
                format!("--scratch={}", ids.join("leases").display()),
                "--cgroup-parent=/kbf.slice/actions".to_owned(),
                format!("--id-files={}", ids.display()),
            ])
            .stderr(std::fs::File::create(&log).expect("stderr file"))
            .spawn()
            .expect("spawn");
        // A daemon that passed the check would run until stopped: bound the wait.
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            if Instant::now() > deadline {
                child.kill().expect("kill");
                child.wait().expect("reap");
                panic!("{name}: the daemon started: {}", read(&log));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.code(), Some(1), "{name}");
        let stderr = read(&log);
        let says = format!("kbf-daemon: {}/{says}", ids.display());
        assert!(stderr.contains(&says), "{name}: {stderr}");
    }
}

/// How long a step of the restart test may take.
const PROMPT: Duration = Duration::from_secs(30);

/// Starts the daemon with `flags`, its log written to `log`.
fn daemon(flags: &[String], log: &Path) -> std::process::Child {
    Command::new(BIN)
        .args(flags)
        .stderr(std::fs::File::create(log).expect("log file"))
        .spawn()
        .expect("spawn")
}

/// Sends `signal` to `child` and reaps it.
fn stop(child: &mut std::process::Child, signal: libc::c_int) -> std::process::ExitStatus {
    let pid = i32::try_from(child.id()).expect("pid");
    // SAFETY: kill(2) on a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    child.wait().expect("wait")
}

/// Whether process `pid` has ended: gone, or a zombie its new parent has yet to reap.
fn ended(pid: i32) -> bool {
    let stat = if cfg!(target_os = "linux") {
        // The state is the first field after the command name, which ends at the last ')'.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| Some(s.rsplit_once(") ")?.1.to_owned()))
            .unwrap_or_default()
    } else {
        let out = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    stat.is_empty() || stat.starts_with('Z')
}

/// Polls `read` until it returns a value, for at most [`PROMPT`].
fn wait_for<T>(what: &str, mut read: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PROMPT;
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Catches (issue #155) a daemon killed by SIGKILL (as the kernel's OOM killer, or a
/// panic that aborts, ends it) whose action outlives it and still runs when the next
/// daemon on the node says `Hello`: the scheduler requeues a lease the new session
/// leaves out, so the action would run twice at once (I12). The action leads its own
/// process group, so neither the dead daemon nor the service manager ends it; the
/// restarted daemon must kill it, and remove its lease directory, before `Hello`.
#[test]
fn a_restarted_daemon_ends_the_runs_it_was_killed_with_before_hello() {
    use std::os::unix::process::ExitStatusExt as _;

    let dir = tls("restart");
    let scratch = dir.join("leases");
    let front = front::Front::start(&dir);
    let action = front.sh("echo $$ > pid; exec sleep 300");
    let flags = vec![
        format!("--server=https://127.0.0.1:{}", front.worker.port()),
        "--tls-server-name=localhost".to_owned(),
        format!("--ca-cert={}", dir.join("ca.pem").display()),
        format!("--cert={}", dir.join("node.pem").display()),
        format!("--key={}", dir.join("node.key").display()),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--driver=native".to_owned(),
        format!("--cas=http://{}", front.cas),
        format!("--scratch={}", scratch.display()),
    ];

    let mut first = daemon(&flags, &dir.join("first.log"));
    let session = front.session(PROMPT);
    session.hello();
    session.welcome();
    session.start(1, 1, action);
    let pid_file = scratch.join("lease-1-1/root/pid");
    let pid: i32 = wait_for("the action's pid", || {
        let text = std::fs::read_to_string(&pid_file).ok()?;
        text.strip_suffix('\n')?.parse().ok()
    });
    assert!(!ended(pid), "the action runs");
    assert_eq!(
        stop(&mut first, libc::SIGKILL).signal(),
        Some(libc::SIGKILL)
    );
    // The premise: nothing else ends the action once its daemon is gone.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!ended(pid), "the action outlived its daemon");

    let mut second = daemon(&flags, &dir.join("second.log"));
    let session = front.session(PROMPT);
    session.hello();
    // What holds as the new session begins.
    let action_ended = ended(pid);
    let lease_dir_left = scratch.join("lease-1-1").exists();
    let status = stop(&mut second, libc::SIGTERM);
    if !action_ended {
        // A red run must not leave the sleep behind. It was alive a moment ago and is
        // no child of anyone who reaps it early, so the pid is still the sleep's.
        // SAFETY: kill(2) takes plain integers.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let log = read(&dir.join("second.log"));
    assert!(
        action_ended,
        "the action (pid {pid}) still ran when the restarted daemon said Hello:\n{log}"
    );
    assert!(
        !lease_dir_left,
        "the lease directory was still there at Hello:\n{log}"
    );
    assert!(status.success(), "{status}: {log}");
}
