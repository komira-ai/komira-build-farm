//! Catches: a `kbf-daemon` binary that fails to start, exits non-zero on `--version`,
//! or reports a name or version other than its own package's; and one that runs
//! without its required flags instead of refusing them.

#[test]
fn prints_name_and_version_and_exits_zero() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_kbf-daemon"))
        .arg("--version")
        .output()
        .expect("spawn kbf-daemon");
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

#[test]
fn refuses_to_run_without_its_flags() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_kbf-daemon"))
        .output()
        .expect("spawn kbf-daemon");
    assert_eq!(out.status.code(), Some(2), "clap's usage error exit code");
    let stderr = String::from_utf8(out.stderr).expect("stderr is UTF-8");
    for flag in [
        "--server",
        "--ca-cert",
        "--cert",
        "--key",
        "--node-id",
        "--runtime",
    ] {
        assert!(
            stderr.contains(flag),
            "{flag} not named as required: {stderr}"
        );
    }
}
