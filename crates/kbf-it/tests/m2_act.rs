//! The `kbf-m2-act` binary's exit codes, which an action's result carries: 0 when the
//! verb ran, 1 when it failed, 2 on a usage error, each failure with a line on stderr.

use std::process::Command;

fn act(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-m2-act"))
        .args(args)
        .output()
        .expect("kbf-m2-act runs")
}

/// Catches: a failure that exits 0 (buck2 would cache a broken output as the
/// action's), a usage error not told apart from a failed verb, and a failure with
/// nothing on stderr (the action's stderr is all a user sees of why).
#[test]
fn exit_codes_say_success_failure_and_usage() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("m2-act");
    std::fs::create_dir_all(&dir).expect("dir");
    let input = dir.join("in");
    let out = dir.join("out");
    std::fs::write(&input, "b\na\n").expect("input");
    let (input, out) = (input.display().to_string(), out.display().to_string());

    let ok = act(&["sort", &out, &input]);
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    assert_eq!(std::fs::read(&out).expect("out"), b"a\nb\n");

    let missing = dir.join("missing").display().to_string();
    let failed = act(&["sort", &out, &missing]);
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("read "));

    let usage = act(&["sort"]);
    assert_eq!(usage.status.code(), Some(2), "{usage:?}");
    assert!(String::from_utf8_lossy(&usage.stderr).contains("usage"));
}
