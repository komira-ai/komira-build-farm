//! The `kbf-mdm-gate` binary: it refuses to start without its configuration. (The
//! library's `config` tests start and stop it in-process, through the same `main`.)

use std::process::Command;

#[test]
fn the_binary_refuses_to_start_without_its_flags() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-mdm-gate"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--allowed-signers"), "{stderr}");
}
