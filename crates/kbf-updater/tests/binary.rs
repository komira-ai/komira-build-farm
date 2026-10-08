//! The `kbf-updater` binary: its flags, and that it refuses to start without a valid
//! root key or beside a daemon binary it cannot read. Serving itself is tested in the
//! library (`server`); no test here installs anything.

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-updater");

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-updater-bin")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Catches: a binary that fails to start, or reports another name or version.
#[test]
fn prints_name_and_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout,
        format!("kbf-updater {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Catches: a pinned value (root key, pool, platform, daemon identity, group) made
/// optional, so a node could start with nothing pinned.
#[test]
fn every_pin_is_a_required_flag() {
    let out = Command::new(BIN).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    for flag in [
        "--root-key-file",
        "--pool",
        "--os",
        "--arch",
        "--daemon-path",
        "--daemon-uid",
        "--socket-gid",
        "--artifacts-dir",
        "--bin-dir",
    ] {
        assert!(stderr.contains(flag), "{flag} not required: {stderr}");
    }
}

/// This runner's `arch` as the capability keys spell it, so the platform pin passes and
/// the start-up check under test is the one that refuses.
fn host_arch() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        std::env::consts::ARCH
    }
}

fn start(dir: &std::path::Path, root_key: &str, daemon: &str) -> (Option<i32>, String) {
    std::fs::write(dir.join("root.pub"), root_key).unwrap();
    let out = Command::new(BIN)
        .args(["--root-key-file", dir.join("root.pub").to_str().unwrap()])
        .args(["--pool", "linux", "--os", "linux", "--arch", host_arch()])
        .args(["--daemon-path", dir.join(daemon).to_str().unwrap()])
        .args(["--daemon-uid", "0", "--socket-gid", "0"])
        .args(["--socket", dir.join("run").join("s").to_str().unwrap()])
        .args(["--state-dir", dir.join("state").to_str().unwrap()])
        .args([
            "--artifacts-dir",
            dir.to_str().unwrap(),
            "--bin-dir",
            dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    (out.status.code(), String::from_utf8(out.stderr).unwrap())
}

/// Catches: a binary that starts with an unreadable root key, or with no daemon binary
/// to check callers against.
#[test]
fn refuses_to_start_without_its_pins() {
    let dir = scratch("refuse");
    let (code, stderr) = start(&dir, "not a key", "kbf-daemon");
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with("kbf-updater: ") && stderr.contains("root.pub"),
        "{stderr}"
    );
    let (code, stderr) = start(&dir, &"ab".repeat(32), "kbf-daemon");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("kbf-daemon"), "{stderr}");
}
