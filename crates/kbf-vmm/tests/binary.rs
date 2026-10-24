//! The `kbf-vmm` binary as a process: what it answers on any platform, what it refuses
//! off macOS, and, on macOS, the commands that need no hypervisor (a missing base bundle,
//! a refused digest). The hypervisor parts run only on a Mac with virtualization.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-vmm");

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run kbf-vmm")
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("binary")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a binary that does not answer `--version` with its name and version and exit
/// status 0: `cargo xtask sign-darwin` runs `<binary> --version` to show the signed
/// binary starts, so such a binary fails the signing job for a reason other than its
/// signature.
#[test]
fn version_prints_name_and_version() {
    let out = run(&["--version"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("kbf-vmm {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Catches a help text that hides a subcommand the docs tell the operator to run, or shows the
/// internal helper mode.
#[test]
fn help_lists_the_commands_and_hides_the_helper_mode() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    for word in ["probe", "probe-bench", "image"] {
        assert!(text.contains(word), "{word}: {text}");
    }
    assert!(!text.contains("vm-run"), "{text}");
    let probe = run(&["probe", "--help"]);
    let text = String::from_utf8_lossy(&probe.stdout);
    for gate in ["g1", "g2", "g3", "g5", "e1", "p0", "p2", "p6"] {
        assert!(text.contains(gate), "{gate}: {text}");
    }
}

/// Catches an unknown verb, no verb or an unknown gate exiting 0 (a caller
/// could take it for a helper that did its work): each exits 2 and says why on stderr.
#[test]
fn bad_command_lines_exit_2() {
    for args in [
        &[][..],
        &["run"][..],
        &["probe", "g9", "--work-dir", "w"][..],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        assert!(!out.stderr.is_empty(), "{args:?}: {out:?}");
    }
}

/// Catches a Linux (or other non-macOS) build that runs a VM command or exits 0 for it:
/// each refuses with status 2, names the command and prints nothing on stdout.
#[cfg(not(target_os = "macos"))]
#[test]
fn every_vm_command_refuses_off_macos() {
    let dir = scratch("refuse");
    let w = dir.to_str().expect("path");
    for (args, name) in [
        (vec!["probe", "g1", "--work-dir", w], "probe"),
        (vec!["probe-bench", "--work-dir", w], "probe-bench"),
        (
            vec![
                "image", "base", "--ipsw", "x.ipsw", "--sha256", "00", "--out", w,
            ],
            "image",
        ),
        (vec!["image", "setup", "--base", w], "image"),
        (
            vec!["image", "harvest", "--bundle", w, "--out", "o"],
            "image",
        ),
        (vec!["vm-run", "--spec", "s.json"], "vm-run"),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{name}: {out:?}");
        assert!(out.stdout.is_empty(), "{name}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("`{name}` needs macOS")),
            "{name}: {err}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&dir).expect("dir").count(),
        0,
        "nothing was created"
    );
}

/// Catches a probe that needs a hypervisor to say it has no base: on a Mac without a
/// base bundle each gate prints needs-manual JSON and exits 0, so the hosted macOS job
/// can run it.
#[cfg(target_os = "macos")]
#[test]
fn a_gate_without_a_base_prints_needs_manual() {
    let dir = scratch("nobase");
    for gate in ["g1", "g2", "g3", "g5", "e1", "p0", "p2", "p6"] {
        let out = run(&["probe", gate, "--work-dir", dir.to_str().expect("path")]);
        assert!(out.status.success(), "{gate}: {out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
        assert_eq!(v["probe"], gate);
        assert_eq!(v["status"], "needs-manual");
        assert!(
            v["reason"]
                .as_str()
                .expect("reason")
                .contains("prepared base")
        );
        assert!(v["host"]["macos"].is_string(), "{v}");
    }
    let out = run(&["probe-bench", "--work-dir", dir.to_str().expect("path")]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["status"], "needs-manual");
}

/// Catches an image build that takes a restore image whose SHA-256 differs from the pin:
/// the process exits 1, names both digests and creates no output.
#[cfg(target_os = "macos")]
#[test]
fn image_base_refuses_a_sha256_mismatch() {
    let dir = scratch("image");
    let ipsw = dir.join("r.ipsw");
    std::fs::write(&ipsw, "hello").expect("ipsw");
    let out_dir = dir.join("bundle");
    let wrong = "0".repeat(64);
    let out = run(&[
        "image",
        "base",
        "--ipsw",
        ipsw.to_str().expect("p"),
        "--sha256",
        &wrong,
        "--out",
        out_dir.to_str().expect("p"),
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["status"], "error");
    let why = v["reason"].as_str().expect("reason");
    assert!(why.contains(&wrong) && why.contains("2cf24dba"), "{why}");
    assert!(!out_dir.exists());
}
