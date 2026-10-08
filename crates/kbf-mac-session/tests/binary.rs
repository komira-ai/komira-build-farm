//! The `kbf-mac-session` binary off macOS: its flags parse, and it refuses to serve.

#![cfg(not(target_os = "macos"))]

use std::process::Command;

fn helper() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kbf-mac-session"))
}

/// Catches: a binary that would bind a socket on a platform whose host it lacks.
#[test]
fn it_refuses_to_serve_off_macos() {
    let output = helper()
        .args(["--daemon-requirement", "cdhash H\"00\""])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("serves only on macOS"), "{stderr}");
}

#[test]
fn the_requirement_flag_is_required() {
    let output = helper().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--daemon-requirement"), "{stderr}");
}
