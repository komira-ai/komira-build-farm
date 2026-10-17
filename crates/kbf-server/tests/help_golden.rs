//! `kbf-server --help` against its golden file, `tests/golden/kbf-server-help.txt`, byte for
//! byte: the binary's command line is public, so a change to its flags or their help
//! shows up here as a diff to review.
//!
//! To rewrite the golden after an intended change:
//! `cargo run -p kbf-server --bin kbf-server -- --help > crates/kbf-server/tests/golden/kbf-server-help.txt`

use std::process::Command;

/// Catches: a flag, a default, a description or the usage line of `kbf-server` changing
/// (or going missing) without the golden file, and with it the reviewer, seeing it.
/// On a mismatch the actual output is printed in full.
#[test]
fn help_matches_the_golden_file() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .arg("--help")
        .output()
        .expect("spawn");
    assert!(out.status.success(), "exit {}", out.status);
    let actual = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let golden = include_str!("golden/kbf-server-help.txt");
    assert!(
        actual == golden,
        "kbf-server --help differs from tests/golden/kbf-server-help.txt; actual:\n{actual}"
    );
}
