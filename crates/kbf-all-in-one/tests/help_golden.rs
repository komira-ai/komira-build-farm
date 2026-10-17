//! `kbf-all-in-one --help` against its golden file, `tests/golden/kbf-all-in-one-help.txt`, byte for
//! byte: the binary's command line is public, so a change to its flags or their help
//! shows up here as a diff to review.
//!
//! To rewrite the golden after an intended change:
//! `cargo run -p kbf-all-in-one --bin kbf-all-in-one -- --help > crates/kbf-all-in-one/tests/golden/kbf-all-in-one-help.txt`

use std::process::Command;

/// Catches: a flag, a default, a description or the usage line of `kbf-all-in-one` changing
/// (or going missing) without the golden file, and with it the reviewer, seeing it.
/// On a mismatch the actual output is printed in full.
#[test]
fn help_matches_the_golden_file() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-all-in-one"))
        .arg("--help")
        .output()
        .expect("spawn");
    assert!(out.status.success(), "exit {}", out.status);
    let actual = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let golden = include_str!("golden/kbf-all-in-one-help.txt");
    assert!(
        actual == golden,
        "kbf-all-in-one --help differs from tests/golden/kbf-all-in-one-help.txt; actual:\n{actual}"
    );
}
