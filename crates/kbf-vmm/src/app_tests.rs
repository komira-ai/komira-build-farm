//! Dispatch: what each command line does with a fake host.

use clap::Parser;
use serde_json::Value;

use crate::app::run;
use crate::cli::{Cli, Cmd, Gate, ImageCmd};
use crate::fake::{FakeSystem, tmp, work_with_base};
use crate::probe::Timing;

fn go(sys: Option<&FakeSystem>, args: &[&str]) -> (String, String, String) {
    let cli = Cli::parse_from(std::iter::once("kbf-vmm").chain(args.iter().copied()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        cli,
        sys.map(|s| s as &dyn crate::system::System),
        Timing::instant(),
        &mut out,
        &mut err,
    );
    (
        format!("{code:?}"),
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    )
}

const OK: &str = "ExitCode(unix_exit_status(0))";
const FAILED: &str = "ExitCode(unix_exit_status(1))";
const REFUSED: &str = "ExitCode(unix_exit_status(2))";

/// Catches a platform with no hypervisor running a subcommand: each one refuses with
/// status 2 and names itself, and prints nothing on stdout.
#[test]
fn without_a_hypervisor_every_subcommand_refuses() {
    for (args, name) in [
        (vec!["probe", "g1", "--work-dir", "/nonexistent"], "probe"),
        (
            vec!["probe-bench", "--work-dir", "/nonexistent"],
            "probe-bench",
        ),
        (
            vec![
                "image",
                "base",
                "--ipsw",
                "x",
                "--sha256",
                "y",
                "--out",
                "/nonexistent/o",
            ],
            "image",
        ),
        (vec!["vm-run", "--spec", "/nonexistent"], "vm-run"),
    ] {
        let (code, out, err) = go(None, &args);
        assert_eq!(code, REFUSED, "{name}");
        assert!(out.is_empty(), "{name}: {out}");
        assert!(
            err.contains(&format!("`{name}` needs macOS")),
            "{name}: {err}"
        );
    }
}

/// Catches arguments that do not reach the gate: the gate name, the work directory, the
/// base and the user id.
#[test]
fn probe_prints_one_json_object_for_the_gate_asked() {
    let work = work_with_base("app-probe");
    let sys = FakeSystem::new();
    let (code, out, _) = go(
        Some(&sys),
        &[
            "probe",
            "G5",
            "--work-dir",
            work.to_str().expect("path"),
            "--run-as-uid",
            "9",
        ],
    );
    assert_eq!(code, OK);
    assert_eq!(out.lines().count(), 1);
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["probe"], "g5");
    assert_eq!(v["host"]["arch"], "arm64");
    assert_eq!(v["details"]["throwaway_uid"], 9);
}

/// Catches `--base` ignored, and a work directory that cannot be made going unreported
/// (exit status 1 with the error as JSON).
#[test]
fn probe_takes_a_base_elsewhere_and_reports_an_unusable_work_directory() {
    let work = work_with_base("app-base");
    let elsewhere = tmp("app-elsewhere");
    let sys = FakeSystem::new();
    let (code, out, _) = go(
        Some(&sys),
        &[
            "probe",
            "p6",
            "--work-dir",
            elsewhere.to_str().expect("p"),
            "--base",
            work.join("base").to_str().expect("p"),
        ],
    );
    assert_eq!(code, OK);
    assert_eq!(
        serde_json::from_str::<Value>(&out).expect("json")["details"]["boot"]["booted"],
        true
    );
    let file = elsewhere.join("afile");
    std::fs::write(&file, "x").expect("file");
    let blocked = file.join("sub");
    let blocked = blocked.to_str().expect("p");
    for args in [
        vec!["probe", "g1", "--work-dir", blocked],
        vec!["probe-bench", "--work-dir", blocked],
    ] {
        let (code, out, _) = go(Some(&sys), &args);
        assert_eq!(code, FAILED, "{args:?}");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["status"], "error");
        assert!(
            v["reason"].as_str().expect("text").contains("create"),
            "{v}"
        );
    }
}

/// Catches `--runs` ignored.
#[test]
fn probe_bench_takes_the_run_count() {
    let work = work_with_base("app-bench");
    let sys = FakeSystem::new();
    for _ in 0..4 {
        sys.script(
            crate::fake::Script::booted()
                .with_guest_file(crate::guest::READY_SYSTEM, "{}")
                .with_guest_file(crate::guest::READY_LOGIN, "{}"),
        );
    }
    let (code, out, _) = go(
        Some(&sys),
        &[
            "probe-bench",
            "--work-dir",
            work.to_str().expect("p"),
            "--runs",
            "2",
        ],
    );
    assert_eq!(code, OK);
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["details"]["warm"]["runs"], 2);
    assert_eq!(sys.starts.borrow().len(), 4);
}

/// Catches the image arguments not reaching the builder, and a refused image exiting 0.
#[test]
fn image_base_passes_its_arguments_and_a_refusal_exits_1() {
    let dir = tmp("app-image");
    let ipsw = dir.join("r.ipsw");
    std::fs::write(&ipsw, "hello").expect("ipsw");
    let sys = FakeSystem::new();
    let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let out_dir = dir.join("b");
    let (code, out, _) = go(
        Some(&sys),
        &[
            "image",
            "base",
            "--ipsw",
            ipsw.to_str().expect("p"),
            "--sha256",
            sha,
            "--out",
            out_dir.to_str().expect("p"),
            "--disk-gib",
            "80",
            "--offline-setup",
        ],
    );
    assert_eq!(code, OK, "{out}");
    assert_eq!(sys.installs.borrow()[0].2, 80);
    assert_eq!(sys.offline_calls.borrow().len(), 1);
    let other = dir.join("c");
    let (code, out, _) = go(
        Some(&sys),
        &[
            "image",
            "base",
            "--ipsw",
            ipsw.to_str().expect("p"),
            "--sha256",
            &"0".repeat(64),
            "--out",
            other.to_str().expect("p"),
        ],
    );
    assert_eq!(code, FAILED);
    assert_eq!(
        serde_json::from_str::<Value>(&out).expect("json")["status"],
        "error"
    );
    assert!(!other.exists());
}

/// Catches the helper mode failing silently: a spec it cannot read or parse, and a
/// helper that errors, exit 1 with the reason on stderr; a good spec reaches the helper.
#[test]
fn vm_run_reads_the_spec_file_and_reports_failures_on_stderr() {
    let dir = tmp("app-vmrun");
    let sys = FakeSystem::new();
    let spec = dir.join("s.json");
    std::fs::write(
        &spec,
        serde_json::to_vec(&crate::spec::VmSpec::minimal("/b".into())).expect("json"),
    )
    .expect("spec");
    let (code, out, err) = go(Some(&sys), &["vm-run", "--spec", spec.to_str().expect("p")]);
    assert_eq!((code.as_str(), out.as_str(), err.as_str()), (OK, "", ""));
    assert_eq!(sys.helper_specs.borrow()[0].bundle.to_str(), Some("/b"));
    *sys.helper.borrow_mut() = Err("config invalid".into());
    let (code, _, err) = go(Some(&sys), &["vm-run", "--spec", spec.to_str().expect("p")]);
    assert_eq!(code, FAILED);
    assert!(err.contains("config invalid"), "{err}");
    let (code, _, err) = go(
        Some(&sys),
        &["vm-run", "--spec", dir.join("none").to_str().expect("p")],
    );
    assert_eq!(code, FAILED);
    assert!(err.contains("read"), "{err}");
    std::fs::write(&spec, "{").expect("bad");
    let (code, _, err) = go(Some(&sys), &["vm-run", "--spec", spec.to_str().expect("p")]);
    assert_eq!(code, FAILED);
    assert!(err.contains("s.json"), "{err}");
}

/// Catches gate names that the parser does not accept in either case, the `p5` alias,
/// and a default run count other than twenty.
#[test]
fn the_command_line_parses_gates_in_any_case_and_defaults_to_twenty_runs() {
    for (name, gate) in [
        ("g1", Gate::G1),
        ("G2", Gate::G2),
        ("g3", Gate::G3),
        ("P5", Gate::G5),
        ("g5", Gate::G5),
        ("E1", Gate::E1),
        ("p0", Gate::P0),
        ("P2", Gate::P2),
        ("p6", Gate::P6),
    ] {
        let cli = Cli::parse_from(["kbf-vmm", "probe", name, "--work-dir", "w"]);
        assert!(
            matches!(&cli.command, Cmd::Probe(a) if a.gate == gate),
            "{name}"
        );
    }
    assert!(Cli::try_parse_from(["kbf-vmm", "probe", "g4", "--work-dir", "w"]).is_err());
    let cli = Cli::parse_from(["kbf-vmm", "probe-bench", "--work-dir", "w"]);
    assert!(matches!(&cli.command, Cmd::ProbeBench(a) if a.runs == 20));
    let cli = Cli::parse_from([
        "kbf-vmm", "image", "base", "--ipsw", "a", "--sha256", "b", "--out", "c",
    ]);
    assert!(matches!(
        &cli.command,
        Cmd::Image(ImageCmd::Base {
            disk_gib: 64,
            offline_setup: false,
            ..
        })
    ));
}

/// Catches `image setup` not reaching the setup step (it needs root, so as a user it says
/// what is missing and exits 0), a `--disk-gib` outside the range the parser accepts, and
/// the subcommand not parsing.
#[test]
fn image_setup_runs_the_offline_write_and_disk_size_is_bounded() {
    let work = work_with_base("app-setup");
    let base = work.join("base");
    let sys = FakeSystem::new();
    let args = ["image", "setup", "--base", base.to_str().expect("p")];
    let (code, out, _) = go(Some(&sys), &args);
    assert_eq!(code, OK);
    assert_eq!(
        serde_json::from_str::<Value>(&out).expect("json")["status"],
        "needs-manual"
    );
    sys.uid.set(0);
    let (code, out, _) = go(Some(&sys), &args);
    assert_eq!(code, OK, "{out}");
    assert_eq!(sys.offline_calls.borrow()[0].0, base.join("disk.img"));
    for bad in ["0", "31", "4097", "-1"] {
        assert!(
            Cli::try_parse_from([
                "kbf-vmm",
                "image",
                "base",
                "--ipsw",
                "a",
                "--sha256",
                "b",
                "--out",
                "c",
                "--disk-gib",
                bad,
            ])
            .is_err(),
            "{bad}"
        );
    }
    for good in ["32", "4096"] {
        assert!(
            Cli::try_parse_from([
                "kbf-vmm",
                "image",
                "base",
                "--ipsw",
                "a",
                "--sha256",
                "b",
                "--out",
                "c",
                "--disk-gib",
                good,
            ])
            .is_ok(),
            "{good}"
        );
    }
}

/// Catches `--graphics` and `--keep` not reaching the probe context (the second VM would
/// stay headless and the clones would be removed), and a harvest whose two paths are
/// swapped on the way to the system.
#[test]
fn e1_flags_and_the_harvest_verb_reach_their_steps() {
    let work = work_with_base("app-e1");
    let sys = FakeSystem::new();
    let bin = work.join("guest-src");
    std::fs::write(&bin, "bin").expect("bin");
    let (code, out, _) = go(
        Some(&sys),
        &[
            "probe",
            "e1",
            "--work-dir",
            work.to_str().expect("p"),
            "--guest-binary",
            bin.to_str().expect("p"),
            "--graphics",
            "--keep",
        ],
    );
    assert_eq!(code, OK, "{out}");
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["probe"], "e1");
    assert_eq!(v["details"]["clones_kept"], true);
    assert!(sys.starts.borrow()[1].0.graphics);
    assert!(work.join("e1-x").is_dir());
    let cli = Cli::parse_from(["kbf-vmm", "probe", "e1", "--work-dir", "w"]);
    assert!(matches!(&cli.command, Cmd::Probe(a) if !a.graphics && !a.keep));
    let bundle = work.join("e1-x");
    let harvested = work.join("h");
    sys.uid.set(0);
    let (code, out, _) = go(
        Some(&sys),
        &[
            "image",
            "harvest",
            "--bundle",
            bundle.to_str().expect("p"),
            "--out",
            harvested.to_str().expect("p"),
        ],
    );
    assert_eq!(code, OK, "{out}");
    assert_eq!(
        sys.harvest_calls.borrow()[0],
        (bundle.join("disk.img"), harvested)
    );
}
