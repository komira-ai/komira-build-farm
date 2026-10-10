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

/// Held by each test while it runs a native daemon, so one runs at a time, as on a
/// node. Native daemons on one Mac share the user's `xcrun` cache, which each `xcrun`
/// lookup rewrites whole: one daemon's lookups (its survey of the Xcodes, then its
/// warm-up) make the other's miss, each then taking seconds, and on the macOS runner
/// a survey took 27.5 s so, against about 4 s alone.
static NATIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`NATIVE`], held until the guard is dropped.
fn one_native_daemon() -> std::sync::MutexGuard<'static, ()> {
    NATIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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

/// The native daemon's line saying its first survey of the Xcodes ended, with how long
/// it took (`took=`).
const SURVEYED: &str = "Xcodes surveyed";

/// How long a native daemon's first survey is waited for, for the CI log only: a
/// survey still running then is logged as such, never failed on (the daemon says
/// Hello without waiting for it, and a cold `xcrun` cache can make it take long).
const SURVEY_LOGGED_WITHIN: Duration = Duration::from_secs(60);

/// Writes to stderr (which tests do not capture), for the CI log, how long the daemon
/// `name` took from its start to `what`, and its first survey's line from `log`.
fn log_start(name: &str, what: &str, took: Duration, log: &[String]) {
    let survey = log
        .iter()
        .find(|l| l.contains(SURVEYED))
        .map_or("no survey ended yet", String::as_str);
    let _ = writeln!(
        std::io::stderr(),
        "binary.rs: the {name} daemon {what} {took:.2?} after it started; {survey}"
    );
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
    // The first attempt is where Hello would go: the native driver does not survey its
    // Xcodes first (its survey runs in the background).
    let tried = started.elapsed();
    let surveying = extra.iter().any(|f| f == "--driver=native");
    let until = Instant::now() + SURVEY_LOGGED_WITHIN;
    while surveying && !log.iter().any(|l| l.contains(SURVEYED)) {
        let Ok(line) = seen.recv_timeout(until.saturating_duration_since(Instant::now())) else {
            break;
        };
        log.push(line);
    }
    log_start(name, "tried its first session", tried, &log);
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
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--scratch={}", scratch.display()),
    ];
    let one = one_native_daemon();
    let status = runs_until_sigterm("native", &native);
    drop(one);
    assert!(status.success(), "{status}");
    assert!(
        scratch.is_dir(),
        "the native driver makes its scratch directory"
    );
    if cfg!(target_os = "linux") {
        // A full range of subordinate ids for this user, whatever the host's files say.
        let ids = id_files("container-ids", "65536");
        let (unit, cgroup_flags) = cgroup_tree(&tls("container-cgroup"), "cpu memory pids");
        let mut container = vec![
            "--driver=container".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            format!("--scratch={}", scratch.display()),
            format!("--id-files={}", ids.display()),
            "--actions-memory-max-gib=1".to_owned(),
        ];
        container.extend(cgroup_flags);
        let (bin, log) = podman_stub("container-bin");
        let status = runs_until_sigterm_with_path("container", &container, Some(&bin));
        assert!(status.success(), "{status}");
        let asked = read(&log);
        assert!(
            asked
                .lines()
                .any(|a| a == "--filter=label=kbf.owner=node-1"),
            "the sweep looks for this node's containers: {asked}"
        );
        // The daemon set up its cgroup (crates/kbf-driver-container/src/delegate.rs).
        assert_eq!(read(&unit.join("supervisor/cgroup.procs")), "1");
        assert_eq!(
            read(&unit.join("cgroup.subtree_control")),
            "+cpu +memory +pids"
        );
        assert_eq!(read(&unit.join("actions/memory.max")), "1073741824");
    }
}

/// A directory put first in `PATH` holding a `podman` that lists no container and
/// logs its arguments to the returned file: the start-up sweep asks it for this
/// node's leftovers before the daemon connects. A symlink, not a written script:
/// exec'ing a file this process just wrote can fail with ETXTBSY while another test
/// thread forks.
fn podman_stub(name: &str) -> (PathBuf, PathBuf) {
    let bin = tls(name);
    let log = bin.join("podman.log");
    let _ = std::fs::remove_file(bin.join("podman"));
    let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/podman-stub.sh");
    std::os::unix::fs::symlink(stub, bin.join("podman")).expect("link podman");
    (bin, log)
}

/// A stand-in cgroup v2 mount in `dir`, of plain directories, for the container
/// driver's setup: the root, and the daemon's unit `/kbf.slice/kbf-daemon.service`,
/// which offers `offered`, holds process 1 and has CPU 0 in its cpuset. Returns the
/// unit's directory and the flags that point the daemon at them (`--cgroup-root`,
/// `--self-cgroup`), so no test touches the host's cgroups.
fn cgroup_tree(dir: &Path, offered: &str) -> (PathBuf, Vec<String>) {
    let root = dir.join("cgroup");
    let unit = root.join("kbf.slice/kbf-daemon.service");
    std::fs::create_dir_all(&unit).expect("mkdir");
    let plant = |path: PathBuf, text: &str| std::fs::write(path, text).expect("plant");
    plant(
        root.join("cgroup.controllers"),
        "cpuset cpu io memory pids\n",
    );
    plant(unit.join("cgroup.controllers"), &format!("{offered}\n"));
    plant(unit.join("cgroup.procs"), "1\n");
    plant(unit.join("cpuset.cpus.effective"), "0\n");
    let own = dir.join("self-cgroup");
    plant(own.clone(), "0::/kbf.slice/kbf-daemon.service\n");
    let flags = vec![
        format!("--cgroup-root={}", root.display()),
        format!("--self-cgroup={}", own.display()),
    ];
    (unit, flags)
}

/// Catches a container node that reports the node's memory and every CPU while its
/// leases may use less (`actions/memory.max`, the cpuset): the scheduler would book
/// past the cap on a host that also runs storage. The `Hello` the front receives says
/// what `actions/` allows: 1 GiB (every runner has more) and CPU 0 alone. Also catches
/// `--supervisor-memory-min-mib` not reaching the daemon's leaf, and a unit whose
/// `memory.min` (0 here) caps it starting without the warning that names `MemoryMin=`.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_reports_what_its_leases_may_use() {
    let dir = tls("capacity");
    let front = front::Front::start(&dir);
    let ids = id_files("capacity-ids", "65536");
    let (unit, cgroup_flags) = cgroup_tree(&dir, "cpu memory pids");
    std::fs::write(unit.join("memory.min"), "0\n").expect("plant");
    let (bin, _) = podman_stub("capacity-bin");
    let log = dir.join("daemon.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(BIN)
        .env("PATH", path)
        .args([
            format!("--server=https://127.0.0.1:{}", front.worker.port()),
            "--tls-server-name=localhost".to_owned(),
            format!("--ca-cert={}", dir.join("ca.pem").display()),
            format!("--cert={}", dir.join("node.pem").display()),
            format!("--key={}", dir.join("node.key").display()),
            "--node-id=node-1".to_owned(),
            "--driver=container".to_owned(),
            format!("--cas=https://127.0.0.1:{}", front.worker.port()),
            format!("--scratch={}", dir.join("leases").display()),
            format!("--id-files={}", ids.display()),
            "--actions-memory-max-gib=1".to_owned(),
            "--supervisor-memory-min-mib=512".to_owned(),
        ])
        .args(cgroup_flags)
        .stderr(std::fs::File::create(&log).expect("log file"))
        .spawn()
        .expect("spawn");
    let session = front.session(PROMPT);
    let hello = session.next(PROMPT);
    let status = stop(&mut child, libc::SIGTERM);
    let log = read(&log);
    let Some(kbf_proto::worker::daemon_message::Message::Hello(hello)) = hello else {
        panic!("expected a Hello, got {hello:?}: {log}");
    };
    let value = |key: &str| -> Vec<&str> {
        hello
            .capabilities
            .iter()
            .filter(|c| c.key == key)
            .map(|c| c.value.as_str())
            .collect()
    };
    assert_eq!(value("mem_gib"), ["1"], "{log}");
    assert_eq!(value("cpus"), ["1"], "{log}");
    assert_eq!(value("drivers"), ["container"], "{log}");
    assert!(status.success(), "{status}: {log}");
    assert_eq!(read(&unit.join("supervisor/memory.min")), "536870912");
    assert!(
        log.contains("memory.min 0 bytes, below the daemon's 536870912")
            && log.contains("set MemoryMin= on its unit and slice to at least 512 MiB"),
        "{log}"
    );
}

/// Catches `--cgroup-parent` set up again by the daemon (it names a cgroup someone else
/// made) or its cap not written: the daemon makes no `supervisor` leaf, and writes the
/// given cgroup's `memory.max` and nothing else.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_given_its_actions_cgroup_writes_only_its_cap() {
    let dir = tls("adopted");
    let ids = id_files("adopted-ids", "65536");
    let (unit, mut flags) = cgroup_tree(&dir, "cpu memory pids");
    let actions = unit.join("actions");
    std::fs::create_dir(&actions).expect("mkdir");
    std::fs::write(actions.join("cgroup.subtree_control"), "cpu memory pids\n").expect("plant");
    flags.extend([
        "--driver=container".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--scratch={}", dir.join("leases").display()),
        format!("--id-files={}", ids.display()),
        "--cgroup-parent=/kbf.slice/kbf-daemon.service/actions".to_owned(),
        "--actions-memory-max-gib=2".to_owned(),
    ]);
    let (bin, _) = podman_stub("adopted-bin");
    let status = runs_until_sigterm_with_path("adopted", &flags, Some(&bin));
    assert!(status.success(), "{status}");
    assert_eq!(read(&actions.join("memory.max")), "2147483648");
    assert!(!unit.join("supervisor").exists());
    assert!(!unit.join("cgroup.subtree_control").exists());
}

/// Catches a container node that starts on a host or under a unit where every lease
/// would fail to make its cgroup, instead of exiting non-zero with the fix: cgroup v1,
/// a unit that does not delegate `memory`, and a cgroup the daemon may not write.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_whose_cgroup_is_not_delegated_refuses_to_start() {
    use std::os::unix::fs::PermissionsExt as _;

    let ids = id_files("undelegated-ids", "65536");
    let refused = |name: &str, offered: &str, prepare: &dyn Fn(&Path, &Path)| {
        let dir = tls(name);
        let (unit, flags) = cgroup_tree(&dir, offered);
        prepare(&dir, &unit);
        let out = Command::new(BIN)
            .args(base(&dir))
            .args([
                "--driver=container".to_owned(),
                "--cas=https://127.0.0.1:1".to_owned(),
                format!("--scratch={}", dir.join("leases").display()),
                format!("--id-files={}", ids.display()),
            ])
            .args(flags)
            .output()
            .expect("spawn");
        let stderr = String::from_utf8(out.stderr).expect("UTF-8");
        assert_eq!(out.status.code(), Some(1), "{name}: {stderr}");
        assert!(!unit.join("actions").exists(), "{name}: actions/ made");
        stderr
    };
    let v1 = |dir: &Path, _: &Path| {
        std::fs::write(dir.join("self-cgroup"), "4:memory:/kbf.slice\n").expect("write");
    };
    let stderr = refused("cgroup-v1", "cpu memory pids", &v1);
    assert!(
        stderr.contains("systemd.unified_cgroup_hierarchy=1"),
        "{stderr}"
    );

    let gone = |dir: &Path, _: &Path| {
        std::fs::remove_file(dir.join("self-cgroup")).expect("remove");
    };
    let stderr = refused("no-self-cgroup", "cpu memory pids", &gone);
    assert!(stderr.contains("self-cgroup: "), "{stderr}");

    let stderr = refused("no-memory", "cpu pids", &|_, _| {});
    assert!(
        stderr.contains("does not offer memory") && stderr.contains("Delegate=yes"),
        "{stderr}"
    );

    // As root, the mode bits do not refuse a write.
    // SAFETY: geteuid(2) takes nothing and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        let read_only = |_: &Path, unit: &Path| {
            std::fs::set_permissions(unit, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        };
        let stderr = refused("read-only", "cpu memory pids", &read_only);
        // Writable again, so the target directory can be cleaned.
        let unit = tls("read-only").join("cgroup/kbf.slice/kbf-daemon.service");
        std::fs::set_permissions(unit, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(stderr.contains("may not write its cgroup"), "{stderr}");
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

/// Catches: a driver started without what it needs (a plain-text `--cas` among
/// them), or a configuration error that does not stop the daemon with a message and a
/// non-zero exit.
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
            vec!["--driver=native".into(), "--cas=https://127.0.0.1:1".into()],
            "--scratch is required",
        ),
        (
            vec![
                "--driver=native".into(),
                "--cas=http://127.0.0.1:1".into(),
                scratch.clone(),
            ],
            "must be an https:// URL",
        ),
        (
            vec!["--driver=container".into(), scratch.clone()],
            if cfg!(target_os = "linux") {
                "--cas is required"
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
                "--cas=https://127.0.0.1:1".to_owned(),
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

/// Waits until the daemon logging to `log` says its first survey ended, for at most
/// [`SURVEY_LOGGED_WITHIN`], and never fails: the line is for the CI log only.
fn wait_logged(log: &Path) {
    let until = Instant::now() + SURVEY_LOGGED_WITHIN;
    while !read(log).contains(SURVEYED) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
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

    let _one = one_native_daemon();
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
        format!("--cas=https://127.0.0.1:{}", front.worker.port()),
        format!("--scratch={}", scratch.display()),
    ];

    let spawned = Instant::now();
    let mut first = daemon(&flags, &dir.join("first.log"));
    let session = front.session(PROMPT);
    session.hello();
    let first_hello = spawned.elapsed();
    session.welcome();
    session.start(1, 1, action);
    let pid_file = scratch.join("lease-1-1/root/pid");
    let pid: i32 = wait_for("the action's pid", || {
        let text = std::fs::read_to_string(&pid_file).ok()?;
        text.strip_suffix('\n')?.parse().ok()
    });
    assert!(!ended(pid), "the action runs");
    // Its first survey's line, for the CI log (this daemon's is the job's first survey
    // of the runner's Xcodes): waited for, never failed on.
    wait_logged(&dir.join("first.log"));
    assert_eq!(
        stop(&mut first, libc::SIGKILL).signal(),
        Some(libc::SIGKILL)
    );
    // The premise: nothing else ends the action once its daemon is gone.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!ended(pid), "the action outlived its daemon");

    let spawned = Instant::now();
    let mut second = daemon(&flags, &dir.join("second.log"));
    let session = front.session(PROMPT);
    session.hello();
    let second_hello = spawned.elapsed();
    // What holds as the new session begins.
    let action_ended = ended(pid);
    let lease_dir_left = scratch.join("lease-1-1").exists();
    wait_logged(&dir.join("second.log"));
    let status = stop(&mut second, libc::SIGTERM);
    if !action_ended {
        // A red run must not leave the sleep behind. It was alive a moment ago and is
        // no child of anyone who reaps it early, so the pid is still the sleep's.
        // SAFETY: kill(2) takes plain integers.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let lines = |file: &str| {
        read(&dir.join(file))
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    log_start(
        "restart test's first",
        "said Hello",
        first_hello,
        &lines("first.log"),
    );
    log_start(
        "restart test's second",
        "said Hello",
        second_hello,
        &lines("second.log"),
    );
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
