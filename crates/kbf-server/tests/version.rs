//! Catches: a `kbf-server` binary that fails to start, exits non-zero, or reports a
//! name or version other than its own package's.

#[test]
fn prints_name_and_version_and_exits_zero() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .output()
        .expect("spawn kbf-server");
    assert!(
        out.status.success(),
        "kbf-server exited with {}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    assert_eq!(
        stdout,
        format!("kbf-server {}\n", env!("CARGO_PKG_VERSION"))
    );
}
