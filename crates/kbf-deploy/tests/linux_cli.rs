//! `kbf-deploy linux render-units` and `check-units` as a user runs them: the lines
//! printed, where, and the exit codes 0, 1 and 2.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXAMPLE: &str = include_str!("../templates/linux/config.example");

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("linux_cli")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn run(args: &[&str]) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(env!("CARGO_BIN_EXE_kbf-deploy"))
        .args(args)
        .output()
        .expect("run kbf-deploy");
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (
        status.code().expect("an exit code"),
        text(stdout),
        text(stderr),
    )
}

fn path(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

#[test]
fn render_then_check_prints_ok_and_exits_0() {
    // Catches: the verbs not wired to the kit, or a success that exits non-zero (an
    // install script would then stop on a good host).
    let dir = scratch("ok");
    let config = dir.join("config.local");
    std::fs::write(&config, EXAMPLE).expect("config");
    let out = dir.join("units");
    let (code, stdout, stderr) = run(&[
        "linux",
        "render-units",
        "--config",
        path(&config),
        "--out",
        path(&out),
        "--kbf-uid",
        "1500",
    ]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert!(
        stdout.starts_with("rendered kbf.slice minio.service.d/kbf.conf "),
        "{stdout}"
    );
    let (code, stdout, _) = run(&[
        "linux",
        "check-units",
        "--config",
        path(&config),
        "--dir",
        path(&out),
        "--installable",
    ]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.starts_with("UNITS OK: MinIO -950 < kbf-server -925"),
        "{stdout}"
    );
}

#[test]
fn a_broken_rule_exits_1_with_its_fail_line() {
    // Catches: a FAIL that exits 0 (the install would go ahead), and the summary
    // printed without the failures that make it.
    let dir = scratch("broken");
    let config = dir.join("config.local");
    std::fs::write(&config, EXAMPLE).expect("config");
    let out = dir.join("units");
    let (code, _, _) = run(&[
        "linux",
        "render-units",
        "--config",
        path(&config),
        "--out",
        path(&out),
    ]);
    assert_eq!(code, 0);
    let (code, stdout, _) = run(&[
        "linux",
        "check-units",
        "--config",
        path(&config),
        "--dir",
        path(&out),
        "--installable",
    ]);
    assert_eq!(code, 1, "{stdout}");
    assert_eq!(
        stdout,
        "FAIL render: kbf-daemon.service keeps a <...> placeholder: <kbf uid>\n\
         UNITS BROKEN: 1 failed check(s)\n"
    );
}

#[test]
fn a_refusal_exits_2_on_stderr_and_writes_nothing() {
    // Catches: a bad setting rendered anyway, a refusal that exits 0 or 1 (1 means
    // "the units are wrong", not "nothing was checked"), and a refusal on stdout,
    // where a caller reading the units' report would take it for one.
    let dir = scratch("refused");
    let config = dir.join("config.local");
    std::fs::write(
        &config,
        EXAMPLE.replace("KBF_RAM_GIB=256", "KBF_RAM_GIB=56"),
    )
    .expect("config");
    let out = dir.join("units");
    let (code, stdout, stderr) = run(&[
        "linux",
        "render-units",
        "--config",
        path(&config),
        "--out",
        path(&out),
    ]);
    assert_eq!((code, stdout.as_str()), (2, ""));
    assert!(
        stderr.starts_with("REFUSED: KBF_RAM_GIB=56 leaves 0 GiB for builds"),
        "{stderr}"
    );
    assert!(!out.exists(), "a refused render wrote {}", out.display());
    let (code, stdout, stderr) = run(&[
        "linux",
        "check-units",
        "--config",
        path(&config),
        "--dir",
        path(&out),
    ]);
    assert_eq!((code, stdout.as_str()), (2, ""));
    assert!(stderr.starts_with("REFUSED: "), "{stderr}");
    std::fs::write(&config, EXAMPLE).expect("config");
    let (code, _, stderr) = run(&[
        "linux",
        "check-units",
        "--config",
        path(&config),
        "--dir",
        path(&out),
    ]);
    assert_eq!(code, 2);
    assert!(stderr.starts_with("REFUSED: cannot read "), "{stderr}");
    let (code, _, _) = run(&["linux", "render-units", "--config", path(&config)]);
    assert_eq!(code, 2, "a missing --out");
}

#[test]
fn a_mode_renders_and_checks_and_an_unknown_mode_is_refused() {
    // Catches: --mode not wired to the mode units (the converged four written
    // instead), a broken mode unit whose check exits 0, and a mode kbf does not have
    // (a typo, an old binary's name) rendered instead of refused with exit 2.
    let dir = scratch("mode");
    let config = dir.join("config.local");
    std::fs::write(&config, EXAMPLE).expect("config");
    let out = dir.join("units");
    let render = |mode: &str| {
        run(&[
            "linux",
            "render-units",
            "--config",
            path(&config),
            "--out",
            path(&out),
            "--kbf-uid",
            "1500",
            "--mode",
            mode,
        ])
    };
    let check = |mode: &str| {
        run(&[
            "linux",
            "check-units",
            "--config",
            path(&config),
            "--dir",
            path(&out),
            "--installable",
            "--mode",
            mode,
        ])
    };
    let (code, stdout, stderr) = render("frontend");
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert!(
        stdout.starts_with("rendered kbf.slice kbf-frontend@.service into "),
        "{stdout}"
    );
    let (code, stdout, _) = check("frontend");
    // The frontend's flags are parsed by the installed kbf, or the check says it found
    // none (a machine without kbf, such as CI): a SKIP line, then the verdict.
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.ends_with("UNIT OK: kbf-frontend@.service runs kbf frontend\n"),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .rev()
            .skip(1)
            .all(|l| l.starts_with("SKIP parse: ")),
        "{stdout}"
    );

    let unit = out.join("kbf-frontend@.service");
    let text = std::fs::read_to_string(&unit).expect("unit");
    let planted = text.replace("TimeoutStopSec=infinity", "TimeoutStopSec=90s");
    assert_ne!(planted, text, "the defect is not planted");
    std::fs::write(&unit, planted).expect("plant");
    let (code, stdout, _) = check("frontend");
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.starts_with("FAIL stop: "), "{stdout}");

    for bad in ["kbf-server", "worker", ""] {
        let (code, stdout, stderr) = render(bad);
        assert_eq!((code, stdout.as_str()), (2, ""), "{bad:?}");
        assert!(
            stderr.contains(
                "[possible values: frontend, storage, daemon, storage-and-daemon, all-in-one]"
            ),
            "{bad:?}: {stderr}"
        );
    }
}
