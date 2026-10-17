//! The `kbf-daemon` binary as a node runs it: its flags, each driver brought up far
//! enough to detect the node and start the session loop, and a clean exit on SIGTERM;
//! and, against a fake front ([`front`]), what a restarted daemon ends before `Hello`.

#[path = "binary/cache.rs"]
mod cache;
mod front;
#[path = "binary/guarded.rs"]
mod guarded;

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};

use guarded::{Guarded, SpawnGuarded as _};

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

/// Catches a daemon binary that does not run as a keeper when the drivers start it as
/// one: one that parses its flags first (clap would refuse `__keeper` and the missing
/// `--driver`), one whose keeper does not write `cgroup.kill` once its pipe closes and
/// then stop after its last try, and one that goes on (or makes the file) once the
/// cgroup is gone.
#[test]
fn the_binary_runs_as_a_keeper_before_it_parses_its_flags() {
    use kbf_daemon::keeper::Mode;

    let dir = tls("keeper");
    let kill = dir.join("cgroup.kill");
    std::fs::write(&kill, "").expect("cgroup.kill");
    keep(Mode::Cgroup {
        kill: kill.clone(),
        tries: 1,
    });
    assert_eq!(read(&kill), "1\n", "no cgroup.kill once its pipe closed");

    // A cgroup the sweep already removed: the keeper stops at once and makes no file.
    let gone = dir.join("gone").join("cgroup.kill");
    let took = keep(Mode::Cgroup {
        kill: gone.clone(),
        tries: 30,
    });
    assert!(!gone.exists(), "the keeper made cgroup.kill");
    assert!(
        took < Duration::from_secs(10),
        "the keeper went on: {took:?}"
    );
}

/// Catches keeper arguments that name no mode taken for the daemon's flags (clap's
/// usage error, about the missing `--driver`) or run as a keeper: the binary exits 2
/// with the keeper's usage line.
#[test]
fn the_binary_refuses_keeper_arguments_that_name_no_mode() {
    let out = Command::new(BIN)
        .args(["__keeper", "groups"])
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.starts_with("kbf-keeper: usage: __keeper group"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty(), "it answered as a keeper");
}

/// Starts the binary as a keeper in `mode`, as the drivers do, checks that it answers
/// as one, closes its pipe and waits for it to exit 0. Returns how long it ran after
/// its pipe closed.
fn keep(mode: kbf_daemon::keeper::Mode) -> Duration {
    use kbf_daemon::keeper;
    use std::os::unix::process::CommandExt as _;

    let mut child = keeper::command(Path::new(BIN), &mode)
        .process_group(0)
        .spawn_guarded()
        .expect("spawn");
    let started = keeper::started(&mut child);
    let closed = Instant::now();
    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    started.expect("the binary answered as a keeper");
    assert!(status.success(), "{status}");
    closed.elapsed()
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

/// What the daemon logs (at WARN) when an attempt to reach a server fails.
const FIRST_ATTEMPT: &str = "cannot reach a server";

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
    runs_until_sigterm_logged(name, extra, path).0
}

/// [`runs_until_sigterm_with_path`], also returning every line the daemon logged.
fn runs_until_sigterm_logged(
    name: &str,
    extra: &[String],
    path: Option<&Path>,
) -> (std::process::ExitStatus, Vec<String>) {
    runs_until_sigterm_logged_then(name, extra, path, || {})
}

/// [`runs_until_sigterm_logged`], running `before_sigterm` once the daemon tried its
/// first session and before it is sent SIGTERM.
fn runs_until_sigterm_logged_then(
    name: &str,
    extra: &[String],
    path: Option<&Path>,
    before_sigterm: impl FnOnce(),
) -> (std::process::ExitStatus, Vec<String>) {
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
        .spawn_guarded()
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
    while !log.iter().any(|l: &String| l.contains(FIRST_ATTEMPT)) {
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
    before_sigterm();
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
        .find(|l| l.contains(FIRST_ATTEMPT))
        .expect("seen");
    assert!(
        ended.contains("connect: transport error: ") && ended.contains("refused"),
        "no cause: {ended}"
    );
    let all = all.into_iter().cloned().collect();
    (status, all)
}

/// The thread flags reach the daemon's runtime: it logs the sizes it resolved and runs
/// exactly that many async workers (the `kbf-async` threads of `/proc`).
///
/// Catches: flags parsed and dropped (the log would show the defaults, the runtime
/// tokio's one worker per CPU), and a start that logs nothing about its sizes.
#[cfg(target_os = "linux")]
#[test]
fn thread_flags_size_the_runtime_and_are_logged() {
    let flags = [
        "--driver=fake",
        "--async-threads=3",
        "--blocking-threads=5",
        "--cpu-threads=2",
    ]
    .map(str::to_owned);
    let mut workers = 0;
    let (status, log) = runs_until_sigterm_logged_then("threads", &flags, None, || {
        // A thread carries its name only once it has run its first instructions, which
        // can be after the start line: wait for the count (up to 10 s).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            workers = kbf_async_threads("--blocking-threads=5");
            if workers == 3 || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    assert!(status.success(), "{status}");
    assert_eq!(workers, 3);
    assert!(
        log.iter()
            .any(|l| l.contains("threads: async=3 blocking=5 cpu=2 reserve=")),
        "{log:#?}"
    );
}

/// The memory backstop is on where the kernel reports memory pressure and off, said
/// so, where it does not (`--psi-memory-file`, which names the file for tests).
///
/// Catches: a backstop that never starts (a node under pressure takes leases without
/// end), one that starts without a readable file (a node on a kernel or a Mac without
/// pressure stall information would be paused or stop), and the threshold flag not
/// reaching it.
#[test]
fn the_memory_backstop_is_on_only_with_a_readable_pressure_file() {
    let dir = tls("psi");
    let file = dir.join("memory.pressure");
    std::fs::write(&file, "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").expect("write");
    let on = [
        "--driver=fake".to_owned(),
        format!("--psi-memory-file={}", file.display()),
        "--memory-pressure-pause-percent=7.5".to_owned(),
    ];
    let (status, log) = runs_until_sigterm_logged("psi", &on, None);
    assert!(status.success(), "{status}");
    assert!(
        log.iter()
            .any(|l| l.contains("memory backstop: on") && l.contains("above_percent=7.5")),
        "{log:#?}"
    );
    assert!(
        log.iter().all(|l| !l.contains("memory backstop: off")),
        "{log:#?}"
    );

    let off = [
        "--driver=fake".to_owned(),
        format!("--psi-memory-file={}", dir.join("missing").display()),
    ];
    let (status, log) = runs_until_sigterm_logged("psi", &off, None);
    assert!(status.success(), "{status}");
    assert!(
        log.iter().any(|l| l.contains("memory backstop: off")),
        "{log:#?}"
    );
    assert!(
        log.iter().all(|l| !l.contains("memory backstop: on")),
        "{log:#?}"
    );
}

/// A pause threshold that is not a percentage above 0 and at most 100 is a usage error
/// (exit 2) naming the flag and the value.
///
/// Catches: a threshold the daemon cannot use starting anyway (0 pauses a node on any
/// stall, 150 never pauses it), and a word taken as a number.
#[test]
fn a_pause_threshold_that_is_no_percentage_is_a_usage_error() {
    let dir = tls("psi-bad");
    for bad in ["0", "150", "lots"] {
        let out = Command::new(BIN)
            .args(base(&dir))
            .args([
                "--driver=fake",
                &format!("--memory-pressure-pause-percent={bad}"),
            ])
            .output()
            .expect("spawn");
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", out.status);
        let stderr = String::from_utf8(out.stderr).expect("stderr is UTF-8");
        assert!(
            stderr.contains("--memory-pressure-pause-percent") && stderr.contains(bad),
            "{bad}: {stderr}"
        );
    }
}

/// A reserve that leaves the daemon no CPU, or is more than the machine has, stops the
/// start with a message naming the flag and the CPUs; a negative one is a usage error.
///
/// Catches: the reserve parsed and never checked (a daemon whose leases get no CPU
/// starts), the check dropped for the fake driver, and a message that does not say what
/// to give less than.
#[test]
fn a_reserve_the_machine_cannot_give_stops_the_start() {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let dir = tls("reserve-refused");
    for reserve in [cpus, cpus + 1] {
        let out = Command::new(BIN)
            .args(base(&dir))
            .args([
                "--driver=fake".to_owned(),
                format!("--reserve-cpus={reserve}"),
            ])
            .output()
            .expect("spawn");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains(&format!("--reserve-cpus={reserve}"))
                && stderr.contains(&format!("{cpus}")),
            "{stderr}"
        );
    }
    let zero = Command::new(BIN)
        .args(base(&dir))
        .args(["--driver=fake", "--reserve-cpus=-1"])
        .output()
        .expect("spawn");
    assert_eq!(zero.status.code(), Some(2), "-1 is a usage error");
}

/// The `kbf-async` threads of the one daemon whose command line holds `flag` (no other
/// test here gives it).
#[cfg(target_os = "linux")]
fn kbf_async_threads(flag: &str) -> usize {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("/proc").flatten() {
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let has = |want: &str| cmdline.split(|b| *b == 0).any(|arg| arg == want.as_bytes());
        if has(flag) && has("--driver=fake") {
            found.push(entry.path());
        }
    }
    assert_eq!(found.len(), 1, "daemons with {flag}: {found:?}");
    std::fs::read_dir(found[0].join("task"))
        .expect("task dir")
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "kbf-async")
        .count()
}

/// Catches: a driver that cannot be brought up from the command line (a missing
/// flag, a scratch directory not made, a node that cannot be detected), and a daemon
/// that does not exit cleanly on SIGTERM.
#[test]
fn each_driver_starts_and_stops_on_sigterm() {
    let status = runs_until_sigterm("fake", &["--driver=fake".to_owned()]);
    assert!(status.success(), "{status}");
    // The fake driver keeps nothing on disk, so without `--data-dir` it touches no
    // directory; given one, it makes it and claims its lease directory.
    let fake_data = tls("fake-data");
    let fake = [
        "--driver=fake".to_owned(),
        format!("--data-dir={}", fake_data.display()),
    ];
    let status = runs_until_sigterm("fake-data", &fake);
    assert!(status.success(), "{status}");
    assert!(
        fake_data.join("leases/daemon-instance").is_file(),
        "the fake daemon with --data-dir makes and claims its lease directory"
    );
    let data = tls("native-scratch");
    let scratch = data.join("leases");
    let native = [
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", data.display()),
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
            format!("--data-dir={}", data.display()),
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

/// A container node started with `--lease-placement=numa-node` over `nodes` (a fake
/// `/sys/devices/system/node`, one `cpulist` each) on the stand-in cgroup mount. Returns
/// the unit's directory after SIGTERM and the daemon's log.
#[cfg(target_os = "linux")]
fn placed_container_node(name: &str, nodes: &[(u32, &str)]) -> (PathBuf, String) {
    let dir = tls(name);
    let numa = dir.join("numa");
    for (id, list) in nodes {
        let node = numa.join(format!("node{id}"));
        std::fs::create_dir_all(&node).expect("mkdir");
        std::fs::write(node.join("cpulist"), format!("{list}\n")).expect("plant");
    }
    let ids = id_files(&format!("{name}-ids"), "65536");
    let (unit, cgroup_flags) = cgroup_tree(&dir, "cpu cpuset memory pids");
    let (bin, _) = podman_stub(&format!("{name}-bin"));
    let mut flags = vec![
        "--driver=container".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", dir.display()),
        format!("--id-files={}", ids.display()),
        "--lease-placement=numa-node".to_owned(),
        format!("--numa-dir={}", numa.display()),
    ];
    flags.extend(cgroup_flags);
    let (status, log) = runs_until_sigterm_logged(name, &flags, Some(&bin));
    let log = log.join("\n");
    assert!(status.success(), "{status}: {log}");
    (unit, log)
}

/// Catches `--lease-placement=numa-node` not reaching the cgroup setup on a two-node
/// host (`cpuset` not enabled for `actions/` and its parent, so no lease cgroup would
/// have cpuset files), and no log line saying the placement is on.
///
/// The cgroup mount is plain directories: the kernel's own rules for `cpuset` are in
/// `delegate_tests.rs`.
#[test]
#[cfg(target_os = "linux")]
fn numa_node_placement_enables_cpuset_on_a_two_node_host() {
    let (unit, log) = placed_container_node("numa-two", &[(0, "0-21,44-65"), (1, "22-43,66-87")]);
    assert_eq!(read(&unit.join("cgroup.subtree_control")), "+cpuset");
    assert_eq!(
        read(&unit.join("actions/cgroup.subtree_control")),
        "+cpuset"
    );
    assert!(
        log.contains("lease placement numa-node: node0=0-21,44-65 node1=22-43,66-87"),
        "{log}"
    );
}

/// Catches a single-node host failing the start or touching its cgroups under
/// `--lease-placement=numa-node`: it runs as `none` and says so once.
#[test]
#[cfg(target_os = "linux")]
fn numa_node_placement_on_a_one_node_host_runs_unplaced() {
    let (unit, log) = placed_container_node("numa-one", &[(0, "0-87")]);
    assert_eq!(
        read(&unit.join("cgroup.subtree_control")),
        "+cpu +memory +pids"
    );
    assert_eq!(log.matches("; not pinned").count(), 1, "{log}");
}

/// `--lease-placement=numa-node` with a driver that runs no containers is refused (also
/// what macOS does: it has no container driver), naming the flag and the driver; the
/// a value that is not a placement is a usage error.
///
/// Catches: the flag silently ignored by a driver that cannot honour it (an operator
/// believes leases are pinned), and a bad value accepted.
#[test]
fn a_placement_without_the_container_driver_is_refused() {
    let dir = tls("placement-refused");
    let out = Command::new(BIN)
        .args(base(&dir))
        .args(["--driver=fake", "--lease-placement=numa-node"])
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("--lease-placement=numa-node") && stderr.contains("--driver container"),
        "{stderr}"
    );
    let bad = Command::new(BIN)
        .args(base(&dir))
        .args(["--driver=fake", "--lease-placement=socket"])
        .output()
        .expect("spawn");
    assert_eq!(
        bad.status.code(),
        Some(2),
        "not a placement is a usage error"
    );
}

/// A directory put first in `PATH` holding a `podman` that lists no container and
/// logs its arguments to the returned file (`kbf-podman-stub`, linked in as
/// `podman`): the start-up sweep asks it for this node's leftovers before the daemon
/// connects. A symlink to the built binary, not a written file: exec'ing a file this
/// process just wrote can fail with ETXTBSY while another test thread forks.
fn podman_stub(name: &str) -> (PathBuf, PathBuf) {
    let bin = tls(name);
    let log = bin.join("podman.log");
    let _ = std::fs::remove_file(bin.join("podman"));
    let stub = env!("CARGO_BIN_EXE_kbf-podman-stub");
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
            format!("--data-dir={}", dir.display()),
            format!("--id-files={}", ids.display()),
            "--actions-memory-max-gib=1".to_owned(),
            "--supervisor-memory-min-mib=512".to_owned(),
        ])
        .args(cgroup_flags)
        .stderr(std::fs::File::create(&log).expect("log file"))
        .spawn_guarded()
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

/// Whatever the driver, the node reports the CPUs its leases get: the CPUs it may use
/// minus `--reserve-cpus` (here the fake driver, which has no cgroup to limit).
///
/// Catches: the reserve applied only where there is a cgroup (a native node would book
/// the whole machine, the reserve included).
#[test]
fn the_hello_reports_the_cpus_minus_the_reserve() {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    assert!(cpus >= 2, "this test needs 2 CPUs to reserve 1");
    let dir = tls("reserve-hello");
    let front = front::Front::start(&dir);
    let mut child = Command::new(BIN)
        .args([
            format!("--server=https://127.0.0.1:{}", front.worker.port()),
            "--tls-server-name=localhost".to_owned(),
            format!("--ca-cert={}", dir.join("ca.pem").display()),
            format!("--cert={}", dir.join("node.pem").display()),
            format!("--key={}", dir.join("node.key").display()),
            "--node-id=node-1".to_owned(),
            "--driver=fake".to_owned(),
            "--reserve-cpus=1".to_owned(),
        ])
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let session = front.session(PROMPT);
    let hello = session.next(PROMPT);
    stop(&mut child, libc::SIGTERM);
    let Some(kbf_proto::worker::daemon_message::Message::Hello(hello)) = hello else {
        panic!("expected a Hello, got {hello:?}");
    };
    let reported: Vec<&str> = hello
        .capabilities
        .iter()
        .filter(|c| c.key == "cpus")
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(reported, [(cpus - 1).to_string()]);
}

/// A container node's leases get the CPUs it may use minus `--reserve-cpus`, and the
/// kernel is told: `actions/cpu.max` is that many CPUs of time (no pinning: no
/// `cpuset.cpus` is written) and the `Hello` reports it as `cpus`.
///
/// Catches: the reserve not reaching the `actions` cgroup (leases could use the whole
/// node, starving the daemon and the server), the quota written in the wrong unit, the
/// reserve taken from the node report only (the kernel would not enforce it), and a
/// cpuset written (pinning).
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_nodes_leases_get_the_cpus_minus_the_reserve() {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    assert!(cpus >= 2, "this test needs 2 CPUs to reserve 1");
    let dir = tls("reserve");
    let front = front::Front::start(&dir);
    let ids = id_files("reserve-ids", "65536");
    let (unit, cgroup_flags) = cgroup_tree(&dir, "cpu memory pids");
    std::fs::remove_file(unit.join("cpuset.cpus.effective")).expect("no cpuset");
    let (bin, _) = podman_stub("reserve-bin");
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
            format!("--data-dir={}", dir.display()),
            format!("--id-files={}", ids.display()),
            "--reserve-cpus=1".to_owned(),
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
    let reported: Vec<&str> = hello
        .capabilities
        .iter()
        .filter(|c| c.key == "cpus")
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(reported, [(cpus - 1).to_string()], "{log}");
    assert!(status.success(), "{status}: {log}");
    assert_eq!(
        read(&unit.join("actions/cpu.max")),
        format!("{} 100000", (cpus - 1) * 100_000)
    );
    assert!(!unit.join("actions/cpuset.cpus").exists(), "pinned: {log}");
    assert!(
        log.contains(&format!("leases may use {} CPUs", cpus - 1)),
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
        format!("--data-dir={}", dir.display()),
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
                format!("--data-dir={}", dir.display()),
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
/// them, the whole-machine driver off macOS or without its helper), a configuration
/// error that does not stop the daemon with a message and a
/// non-zero exit, and an `--image` that a driver other than the container driver
/// takes and ignores (the node would list images no lease of it can run).
#[test]
fn a_driver_missing_its_flags_refuses_to_start() {
    let dir = tls("refused");
    let scratch = format!("--data-dir={}", dir.display());
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec!["--driver=native".into(), scratch.clone()],
            "--cas is required",
        ),
        (
            vec![
                "--driver=native".into(),
                "--cas=https://127.0.0.1:1".into(),
                "--data-dir=relative".into(),
            ],
            "--data-dir relative must be absolute",
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
            vec![
                "--driver=native".into(),
                format!("--image={}", image_by("0")),
            ],
            "only --driver container runs them",
        ),
        (
            vec!["--driver=fake".into(), format!("--image={}", image_by("0"))],
            "only --driver container runs them",
        ),
        (
            vec!["--driver=container".into(), scratch.clone()],
            if cfg!(target_os = "linux") {
                "--cas is required"
            } else {
                "runs on Linux only"
            },
        ),
        // The whole-machine runtime needs a Mac and its root helper; a node without
        // them must not list native-whole-machine.
        (
            vec![
                "--driver=native-whole-machine".into(),
                "--cas=https://127.0.0.1:1".into(),
                scratch.clone(),
                format!("--mac-session-socket={}", dir.join("no-helper").display()),
            ],
            if cfg!(target_os = "macos") {
                "needs kbf-mac-session"
            } else {
                "runs on macOS only"
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
                format!("--data-dir={}", ids.display()),
                "--cgroup-parent=/kbf.slice/actions".to_owned(),
                format!("--id-files={}", ids.display()),
            ])
            .stderr(std::fs::File::create(&log).expect("stderr file"))
            .spawn_guarded()
            .expect("spawn");
        // A daemon that passed the check would run until stopped: bound the wait.
        let status = refused_within(&mut child, name, &log);
        assert_eq!(status.code(), Some(1), "{name}");
        let stderr = read(&log);
        let says = format!("kbf-daemon: {}/{says}", ids.display());
        assert!(stderr.contains(&says), "{name}: {stderr}");
    }
}

/// Catches `--container-runtime` and `--oci-runtime` not reaching the driver: a node told
/// to run actions through a runtime that is not there refuses to start, naming it (the
/// "ignore the flag" mutant would start a Podman node), and a runtime that is neither
/// `podman` nor `oci` is refused by the parser.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_node_told_to_use_the_oci_backend_needs_its_runtime() {
    let ids = id_files("oci-ids", "65536");
    let (_unit, cgroup_flags) = cgroup_tree(&tls("oci-cgroup"), "cpu memory pids");
    let absent = ids.join("no-such-crun");
    let log = ids.join("stderr");
    let mut container = vec![
        "--driver=container".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", ids.display()),
        format!("--id-files={}", ids.display()),
        "--actions-memory-max-gib=1".to_owned(),
        "--container-runtime=oci".to_owned(),
        format!("--oci-runtime={}", absent.display()),
    ];
    container.extend(cgroup_flags);
    let mut child = Command::new(BIN)
        .args(base(&ids))
        .args(&container)
        .stderr(std::fs::File::create(&log).expect("stderr file"))
        .spawn()
        .expect("spawn");
    let status = refused_within(&mut child, "oci", &log);
    assert_eq!(status.code(), Some(1));
    let says = format!("the OCI backend: {} is not a file", absent.display());
    let stderr = read(&log);
    assert!(stderr.contains(&says), "{stderr}");

    let out = Command::new(BIN)
        .args(base(&ids))
        .args(["--driver=fake", "--container-runtime=runc"])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("possible values: podman, oci"), "{stderr}");
}

/// The exit status of `child`, a daemon expected to refuse to start, waited for up to
/// 30 s. A daemon still running then has started: it is killed and the test fails,
/// naming `what` and the log `child` writes to `log`.
fn refused_within(
    child: &mut std::process::Child,
    what: &str,
    log: &Path,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().expect("kill");
            child.wait().expect("reap");
            panic!("{what}: the daemon started: {}", read(log));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Catches the `--container-*` limits not reaching the container driver's
/// configuration (`config.limits = limits(cli)` in `container::start`): the driver
/// would create every container with `ContainerLimits::DEFAULT` whatever the flags
/// say. The driver refuses a limit of 0 before it asks Podman anything, so a daemon
/// given one 0 limit exits non-zero naming that limit; a daemon whose flags never
/// reach the driver gets the defaults, starts, and is killed after 30 s. One case per
/// limit, so a flag left out of the wiring is caught on its own.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_given_a_zero_limit_refuses_to_start() {
    let ids = id_files("zero-limit-ids", "65536");
    for (flag, name) in [
        ("--container-pids-limit=0", "pids"),
        ("--container-shm-mib=0", "shm"),
        ("--container-nofile=0", "nofile"),
        ("--container-nproc=0", "nproc"),
    ] {
        let dir = tls(&format!("zero-{name}"));
        let (unit, cgroup_flags) = cgroup_tree(&dir, "cpu memory pids");
        let (bin, podman_log) = podman_stub(&format!("zero-{name}-bin"));
        let log = dir.join("stderr");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(BIN)
            .env("PATH", path)
            .args(base(&dir))
            .args([
                "--driver=container".to_owned(),
                "--cas=https://127.0.0.1:1".to_owned(),
                format!("--data-dir={}", dir.display()),
                format!("--id-files={}", ids.display()),
                flag.to_owned(),
            ])
            .args(cgroup_flags)
            .stderr(std::fs::File::create(&log).expect("stderr file"))
            .spawn_guarded()
            .expect("spawn");
        let status = refused_within(&mut child, flag, &log);
        let stderr = read(&log);
        assert_eq!(status.code(), Some(1), "{flag}: {stderr}");
        let says = format!("kbf-daemon: the container {name} limit must be at least 1");
        assert!(stderr.contains(&says), "{flag}: {stderr}");
        // The limits are checked after the cgroup setup and before the start-up
        // sweep: `actions/` is made, and Podman was never asked for leftovers.
        assert!(unit.join("actions").is_dir(), "{flag}: {stderr}");
        assert!(!podman_log.exists(), "{flag}: podman ran: {stderr}");
    }
}

/// How long a step of the restart test may take.
const PROMPT: Duration = Duration::from_secs(30);

/// Starts the daemon with `flags`, its log written to `log`.
fn daemon(flags: &[String], log: &Path) -> Guarded {
    Command::new(BIN)
        .args(flags)
        .stderr(std::fs::File::create(log).expect("log file"))
        .spawn_guarded()
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
    stat_is_ended(&stat)
}

/// Whether a process whose state field (what follows its command name in
/// `/proc/<pid>/stat`, or `ps -o stat=`) is `stat` has ended: it is gone (no field), a
/// zombie (`Z`) or dead (`X`, or `x` on kernels before 3.13). A zombie that its new
/// parent reaps shows `X` for the instant the reaper holds it, so `X` must count: read
/// as alive, a process that had ended turns up as a survivor on the next look.
fn stat_is_ended(stat: &str) -> bool {
    stat.is_empty() || stat.starts_with(['Z', 'X', 'x'])
}

/// Catches an `ended` that calls a process alive while its reaper holds it: the state
/// `X` follows `Z` for an instant before the pid is gone, so a process seen ended once
/// (as `Z`) was a survivor on the next look (the flake of
/// [`a_killed_daemons_action_ends_without_a_restart`]). Every state field that means
/// the process is still there must stay alive.
#[test]
fn a_process_whose_reaper_holds_it_has_ended() {
    for stat in ["", "Z", "Z 1 2 3", "X", "X 1 2 3", "x"] {
        assert!(stat_is_ended(stat), "{stat:?} has ended");
    }
    for stat in ["R", "S 1 2 3", "D", "T", "t", "I", "Ss+"] {
        assert!(!stat_is_ended(stat), "{stat:?} is still there");
    }
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

/// The flags of a native daemon in `dir` that serves `front`, with leases under
/// `scratch` (`<--data-dir>/leases`: `scratch`'s parent is the data directory).
fn native_flags(front: &front::Front, dir: &Path, scratch: &Path) -> Vec<String> {
    vec![
        format!("--server=https://127.0.0.1:{}", front.worker.port()),
        "--tls-server-name=localhost".to_owned(),
        format!("--ca-cert={}", dir.join("ca.pem").display()),
        format!("--cert={}", dir.join("node.pem").display()),
        format!("--key={}", dir.join("node.key").display()),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--driver=native".to_owned(),
        format!("--cas=https://127.0.0.1:{}", front.worker.port()),
        format!(
            "--data-dir={}",
            scratch.parent().expect("a data dir").display()
        ),
    ]
}

/// Reads the pid that the action wrote to `file`, waiting for it.
fn pid_in(file: &Path) -> i32 {
    wait_for(&format!("the pid in {}", file.display()), || {
        let text = std::fs::read_to_string(file).ok()?;
        text.strip_suffix('\n')?.parse().ok()
    })
}

/// Catches (issue #155) a killed daemon's action that runs on until the daemon is
/// started again, for as long as the work takes, or for ever when it is not: the
/// scheduler requeues the lease after the handover grace, so the work runs twice. With
/// no daemon started again, both the action's leader and a process it started must end
/// within [`PROMPT`] of the SIGKILL.
#[test]
fn a_killed_daemons_action_ends_without_a_restart() {
    use std::os::unix::process::ExitStatusExt as _;

    let _one = one_native_daemon();
    let dir = tls("killed");
    let scratch = dir.join("leases");
    let front = front::Front::start(&dir);
    let action = front.user_shell("echo $$ > leader; sleep 300 & echo $! > child; wait");
    let mut daemon = daemon(
        &native_flags(&front, &dir, &scratch),
        &dir.join("daemon.log"),
    );
    let session = front.session(PROMPT);
    session.hello();
    session.welcome();
    session.start(1, 1, action);
    let root = scratch.join("lease-1-1/root");
    let (leader, child) = (pid_in(&root.join("leader")), pid_in(&root.join("child")));
    assert!(!ended(leader) && !ended(child), "the action runs");
    wait_logged(&dir.join("daemon.log"));
    assert_eq!(
        stop(&mut daemon, libc::SIGKILL).signal(),
        Some(libc::SIGKILL)
    );
    let deadline = Instant::now() + PROMPT;
    while !(ended(leader) && ended(child)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survivors: Vec<i32> = [leader, child].into_iter().filter(|&p| !ended(p)).collect();
    // A red run must not leave the action behind: the group is still the action's, as
    // its leader is alive or has been reaped by no one who would reuse its pid soon.
    if !survivors.is_empty() {
        // SAFETY: kill(2) takes plain integers; the group is the action's (above).
        unsafe { libc::kill(-leader, libc::SIGKILL) };
    }
    assert!(
        survivors.is_empty(),
        "processes of the action {survivors:?} outlived the killed daemon by {PROMPT:?}:\n{}",
        read(&dir.join("daemon.log"))
    );
}

/// Catches a lease whose output upload waits for ever on a store that stopped
/// answering: the worker listener's `Write` takes the upload and never replies, as a
/// stalled object store behind it did for hours. With
/// `--output-upload-deadline-secs=2` the lease must end within [`UPLOAD_HANG`] with
/// INTERNAL and a reason naming the upload and its deadline; without the deadline no
/// Result comes at all.
#[test]
fn a_lease_whose_upload_stalls_fails_at_its_deadline() {
    let _one = one_native_daemon();
    let dir = tls("stalled-upload");
    let scratch = dir.join("leases");
    let front = front::Front::stalling_writes(&dir);
    // Its stdout is a blob to write: the upload cannot be skipped as already stored.
    let action = front.user_shell("echo the action ran");
    let mut flags = native_flags(&front, &dir, &scratch);
    flags.push("--output-upload-deadline-secs=2".to_owned());
    let log = dir.join("daemon.log");
    let mut daemon = daemon(&flags, &log);
    let session = front.session(PROMPT);
    session.hello();
    session.welcome();
    session.start(1, 1, action);
    let result = session.result(UPLOAD_HANG);
    // SIGKILL: a daemon whose lease hangs in its upload may not end on SIGTERM.
    stop(&mut daemon, libc::SIGKILL);
    let Some(result) = result else {
        panic!(
            "no Result within {UPLOAD_HANG:?}: the upload hangs\n{}",
            read(&log)
        );
    };
    let status = result.status.expect("a status");
    assert_eq!(status.code, tonic::Code::Internal as i32, "{status:?}");
    assert!(
        status
            .message
            .starts_with("the output upload did not finish within its deadline of 2s"),
        "{status:?}"
    );
}

/// How long [`a_lease_whose_upload_stalls_fails_at_its_deadline`] waits for the Result
/// of a lease whose upload has a 2 s deadline: the action and its inputs take well
/// under a second, so a minute only fails a lease that hangs.
const UPLOAD_HANG: Duration = Duration::from_secs(60);

/// The live processes other than `pgid` itself in process group `pgid`, from `ps`.
fn others_in_group(pgid: i32) -> Vec<i32> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "pgid="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<i32>);
            Some((fields.next()?.ok()?, fields.next()?.ok()?))
        })
        .filter(|&(pid, group)| group == pgid && pid != pgid && !ended(pid))
        .map(|(pid, _)| pid)
        .collect()
}

/// Catches (issue #155) a daemon killed by SIGKILL (as the kernel's OOM killer, or a
/// panic that aborts, ends it) whose action outlives it and still runs when the next
/// daemon on the node says `Hello`: the scheduler requeues a lease the new session
/// leaves out, so the action would run twice at once (I12). The action leads its own
/// process group, so neither the dead daemon nor the service manager ends it; with its
/// keeper killed too, the restarted daemon must kill it, and remove its lease
/// directory, before `Hello`.
#[test]
fn a_restarted_daemon_ends_the_runs_it_was_killed_with_before_hello() {
    use std::os::unix::process::ExitStatusExt as _;

    let _one = one_native_daemon();
    let dir = tls("restart");
    let scratch = dir.join("leases");
    let front = front::Front::start(&dir);
    let action = front.user_shell("echo $$ > pid; exec sleep 300");
    let flags = native_flags(&front, &dir, &scratch);

    let spawned = Instant::now();
    let mut first = daemon(&flags, &dir.join("first.log"));
    let session = front.session(PROMPT);
    let first_said = session.hello();
    let first_hello = spawned.elapsed();
    session.welcome();
    session.start(1, 1, action);
    let pid_file = scratch.join("lease-1-1/root/pid");
    let pid = pid_in(&pid_file);
    assert!(!ended(pid), "the action runs");
    // Its first survey's line, for the CI log (this daemon's is the job's first survey
    // of the runner's Xcodes): waited for, never failed on.
    wait_logged(&dir.join("first.log"));
    // The keeper is the action's group's one other member: killed first, as the
    // kernel's OOM killer may take it with the daemon, it leaves the sweep to act.
    let keepers = others_in_group(pid);
    for &keeper in &keepers {
        // SAFETY: kill(2) on the keeper, the daemon's unreaped child, so the pid is its.
        assert_eq!(unsafe { libc::kill(keeper, libc::SIGKILL) }, 0);
    }
    wait_for("the keeper's end", || {
        keepers.iter().all(|&k| ended(k)).then_some(())
    });
    assert_eq!(
        stop(&mut first, libc::SIGKILL).signal(),
        Some(libc::SIGKILL)
    );
    // The premise: nothing else ends the action once its daemon and keeper are gone.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!ended(pid), "the action outlived its daemon");

    let spawned = Instant::now();
    let mut second = daemon(&flags, &dir.join("second.log"));
    let session = front.session(PROMPT);
    let second_said = session.hello();
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
    assert_eq!(keepers.len(), 1, "one keeper: {keepers:?}");
    assert!(
        action_ended,
        "the action (pid {pid}) still ran when the restarted daemon said Hello:\n{log}"
    );
    assert!(
        !lease_dir_left,
        "the lease directory was still there at Hello:\n{log}"
    );
    assert!(status.success(), "{status}: {log}");
    // Issue #22: the second daemon names the first, whose runs it killed, so the server
    // reruns the first's leases at once; the first, on a fresh scratch, names none.
    assert_eq!(first_said.predecessor_instance_id, "");
    assert_eq!(second_said.predecessor_instance_id, first_said.instance_id);
    assert_ne!(second_said.instance_id, first_said.instance_id);
}

/// The per-architecture manifest the stub's image store holds for a pinned image.
#[cfg(target_os = "linux")]
const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
/// An image index (a multi-architecture list).
#[cfg(target_os = "linux")]
const INDEX: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;

/// `--image`'s value for the busybox image whose manifest is `bytes`.
#[cfg(target_os = "linux")]
fn image_of(bytes: &[u8]) -> String {
    image_by(&kbf_daemon::cas::digest_of(bytes).hash)
}

/// `--image`'s value for the busybox image with manifest digest `sha256:<hex>`.
fn image_by(hex: &str) -> String {
    format!("docker://registry.test/tools/busybox@sha256:{hex}")
}

/// Puts `bytes` in the stub's image store (see `src/bin/kbf-podman-stub.rs`) as image
/// `img1`'s manifest, under the digest of `bytes`, and makes the stub know `img1`.
#[cfg(target_os = "linux")]
fn store_manifest(bin: &Path, bytes: &[u8]) {
    let digest = format!("sha256:{}", kbf_daemon::cas::digest_of(bytes).hash);
    let dir = bin.join("store/img1");
    std::fs::create_dir_all(&dir).expect("mkdir store");
    let file = kbf_driver_container::image::manifest_file(&digest);
    std::fs::write(dir.join(file), bytes).expect("write manifest");
    std::fs::write(bin.join("image-id"), "img1\n").expect("image id");
}

/// The container driver's flags for the daemon in `dir`, against the server `base`
/// names, with a delegated stand-in cgroup and `--image` once per entry of `images`.
#[cfg(target_os = "linux")]
fn container_flags(dir: &Path, ids: &Path, images: &[String]) -> Vec<String> {
    let (_, cgroup_flags) = cgroup_tree(dir, "cpu memory pids");
    let mut flags = vec![
        "--driver=container".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", dir.display()),
        format!("--id-files={}", ids.display()),
    ];
    flags.extend(cgroup_flags);
    flags.extend(images.iter().map(|i| format!("--image={i}")));
    flags
}

/// Catches a container node that starts while an image it is pinned to (`--image`)
/// cannot run on it, instead of exiting with status 2 and the fix: an image missing
/// from its store started anyway (warned about and continued), its refusal without the
/// `podman pull <repo>@sha256:<digest>` that fixes it, an image index digest taken for
/// one architecture's manifest (`manifest_kind` not asked), a tag accepted, only the
/// first `--image` checked (a good image first, an index second), and only the last
/// one deciding (an index first, a good image second). Each daemon
/// still running after 30 s has started, and fails the test. Linux only, as the
/// driver is.
#[cfg(target_os = "linux")]
#[test]
fn a_container_node_refuses_to_start_on_an_image_it_cannot_run() {
    let ids = id_files("image-refused-ids", "65536");
    let digest = kbf_daemon::cas::digest_of(MANIFEST).hash;
    let tagged = format!("docker://registry.test/tools/busybox:1@sha256:{digest}");
    let index = "names an image index; name the per-architecture manifest digest";
    /// (name, the images, what the stub's store holds, what the refusal says)
    type Case = (&'static str, Vec<String>, &'static [&'static [u8]], String);
    let cases: [Case; 5] = [
        (
            "image-missing",
            vec![image_of(MANIFEST)],
            &[],
            format!("podman pull registry.test/tools/busybox@sha256:{digest}"),
        ),
        (
            "image-index",
            vec![image_of(INDEX)],
            &[INDEX],
            index.to_owned(),
        ),
        (
            "image-tag",
            vec![tagged],
            &[MANIFEST],
            "names a tag".to_owned(),
        ),
        (
            "image-second",
            vec![image_of(MANIFEST), image_of(INDEX)],
            &[MANIFEST, INDEX],
            index.to_owned(),
        ),
        (
            "image-first",
            vec![image_of(INDEX), image_of(MANIFEST)],
            &[INDEX, MANIFEST],
            index.to_owned(),
        ),
    ];
    for (name, images, stored, says) in cases {
        let dir = tls(name);
        let (bin, _) = podman_stub(&format!("{name}-bin"));
        for bytes in stored {
            store_manifest(&bin, bytes);
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let log = dir.join("daemon.log");
        let mut child = Command::new(BIN)
            .env("PATH", path)
            .args(base(&dir))
            .args(container_flags(&dir, &ids, &images))
            .stderr(std::fs::File::create(&log).expect("log file"))
            .spawn_guarded()
            .expect("spawn");
        let status = refused_within(&mut child, name, &log);
        let log = read(&log);
        assert_eq!(status.code(), Some(2), "{name}: {log}");
        assert!(log.contains(&says), "{name}: {log}");
        assert!(
            !log.contains(FIRST_ATTEMPT),
            "{name}: it tried a session: {log}"
        );
    }
}

/// Catches the images a container node is pinned to (`--image`) missing from the
/// `NodeStatus` it sends after the Welcome (`GET /v1/nodes` would not show what the
/// node can run), listed unsorted or more than once, and a node whose images are all
/// in its store, as manifests, refusing to start. Three images given out of order, the
/// first one twice, with distinct images at both ends: a node that drops the first or
/// the last `--image` from its status fails the set.
#[cfg(target_os = "linux")]
#[test]
fn a_container_node_lists_its_pinned_images_in_its_status() {
    let dir = tls("image-listed");
    let front = front::Front::start(&dir);
    let ids = id_files("image-listed-ids", "65536");
    let (bin, _) = podman_stub("image-listed-bin");
    let other = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]}"#;
    let third = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;
    store_manifest(&bin, MANIFEST);
    store_manifest(&bin, other);
    store_manifest(&bin, third);
    let mut images = [image_of(MANIFEST), image_of(other), image_of(third)];
    images.sort();
    let given = [
        images[2].clone(),
        images[0].clone(),
        images[0].clone(),
        images[1].clone(),
    ];
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log = dir.join("daemon.log");
    let mut flags = vec![
        format!("--server=https://127.0.0.1:{}", front.worker.port()),
        "--tls-server-name=localhost".to_owned(),
        format!("--ca-cert={}", dir.join("ca.pem").display()),
        format!("--cert={}", dir.join("node.pem").display()),
        format!("--key={}", dir.join("node.key").display()),
        "--node-id=node-1".to_owned(),
    ];
    flags.extend(container_flags(&dir, &ids, &given));
    let mut child = Command::new(BIN)
        .env("PATH", path)
        .args(flags)
        .stderr(std::fs::File::create(&log).expect("log file"))
        .spawn_guarded()
        .expect("spawn");
    let session = front.session(PROMPT);
    session.hello();
    session.welcome();
    // After a Welcome the node reports how it updates (a node with no --update-dir says
    // MANUAL); the NodeStatus is the next message that is not that.
    let status = std::iter::from_fn(|| Some(session.next(PROMPT)))
        .find(|m| {
            !matches!(
                m,
                Some(kbf_proto::worker::daemon_message::Message::UpdateStatus(_))
            )
        })
        .flatten();
    let stopped = stop(&mut child, libc::SIGTERM);
    let log = read(&log);
    let Some(kbf_proto::worker::daemon_message::Message::NodeStatus(status)) = status else {
        panic!("expected a NodeStatus, got {status:?}: {log}");
    };
    assert_eq!(status.container_images, images, "{log}");
    assert!(stopped.success(), "{stopped}: {log}");
}
