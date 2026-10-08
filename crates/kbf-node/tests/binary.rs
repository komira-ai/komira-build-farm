//! The `kbf-daemon` binary as a node runs it: its flags, each driver brought up far
//! enough to detect the node and start the session loop, and a clean exit on SIGTERM.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-daemon");

/// Catches: a binary that fails to start, exits non-zero on `--version`, or reports a
/// name or version other than its own package's.
#[test]
fn prints_name_and_version_and_exits_zero() {
    let out = Command::new(BIN).arg("--version").output().expect("spawn");
    assert!(
        out.status.success(),
        "kbf-daemon exited with {}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    assert_eq!(
        stdout,
        format!("kbf-daemon {}\n", env!("CARGO_PKG_VERSION"))
    );
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

/// Starts the daemon with `extra` flags, waits until its session loop has failed to
/// connect once (so detection, the driver and the TLS files all worked), then sends
/// SIGTERM and returns its exit status.
fn runs_until_sigterm(name: &str, extra: &[String]) -> std::process::ExitStatus {
    let dir = tls(name);
    let mut child = Command::new(BIN)
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
        let status = runs_until_sigterm("container", &container);
        assert!(status.success(), "{status}");
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
