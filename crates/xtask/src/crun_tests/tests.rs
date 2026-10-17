use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::*;

#[test]
fn the_machine_names_its_architecture_and_an_unknown_one_is_refused() {
    // Catches the two mapped the wrong way round, or one architecture for every
    // machine (busybox for the wrong one pulls, and the tests run another image than
    // the one named).
    assert_eq!(arch_for("x86_64"), Ok("amd64"));
    assert_eq!(arch_for("aarch64"), Ok("arm64"));
    // Catches a machine with no mapping given a default architecture: macOS's spelling
    // of arm64, a 32-bit ARM board and an empty answer are all refused.
    for machine in ["arm64", "armv7l", ""] {
        assert_eq!(
            arch_for(machine),
            Err(Error::Failed(format!(
                "no busybox architecture for {machine}"
            )))
        );
    }
}

#[test]
fn each_architecture_has_its_own_pinned_manifest() {
    // Catches the two pins swapped, one digest for every machine, a pin that is not a
    // sha256 digest (the image would not resolve) and a pin equal to the index's (the
    // index test would run the image the manifest test runs, and prove nothing); and an
    // architecture with no pin given some digest.
    assert_eq!(manifest_for("amd64"), Ok(BUSYBOX_AMD64));
    assert_eq!(manifest_for("arm64"), Ok(BUSYBOX_ARM64));
    assert_ne!(BUSYBOX_AMD64, BUSYBOX_ARM64);
    for pin in [BUSYBOX_AMD64, BUSYBOX_ARM64, BUSYBOX_INDEX] {
        let hex = pin.strip_prefix("sha256:").expect("a sha256 digest");
        assert_eq!(hex.len(), 64, "{pin}");
        assert!(
            hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{pin}"
        );
    }
    assert_ne!(BUSYBOX_AMD64, BUSYBOX_INDEX);
    assert_ne!(BUSYBOX_ARM64, BUSYBOX_INDEX);
    for arch in ["arm", "s390x", ""] {
        assert_eq!(
            manifest_for(arch),
            Err(Error::Failed(format!(
                "no pinned {BUSYBOX_NAME} manifest for linux/{arch}"
            )))
        );
    }
}

/// A scratch directory for one unit test, empty.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-pt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

#[test]
fn the_host_report_reads_each_file_and_marks_a_missing_one() {
    // Catches a file read from the wrong path, a value not trimmed, and a missing file
    // failing the verb (the script printed `n/a`; lru_gen and zswap are not on every
    // kernel).
    let root = scratch("report");
    let files = [
        ("proc/sys/kernel/osrelease", "6.8.0-1017-generic\n"),
        ("proc/sys/vm/swappiness", "60\n"),
        ("sys/module/zswap/parameters/enabled", "N\n"),
        (
            "sys/kernel/mm/transparent_hugepage/enabled",
            "always [madvise] never\n",
        ),
        (
            "proc/swaps",
            "Filename\tType\tSize\tUsed\tPriority\n/mnt/swapfile\tfile\t4194300\t0\t-2\n",
        ),
    ];
    for (file, text) in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("file");
    }
    assert_eq!(
        host_report(&root),
        "kernel 6.8.0-1017-generic; vm.swappiness 60; zswap N; lru_gen n/a; THP always \
         [madvise] never\nswap areas:\nFilename\tType\tSize\tUsed\tPriority\n/mnt/swapfile\t\
         file\t4194300\t0\t-2"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The binary cargo built for `target`, in a package at `/w/crates/kbf-driver-container`.
fn built(target: &Target) -> Built {
    Built {
        executable: format!("/w/target/debug/deps/{}-0f1e", target.name).into(),
        dir: "/w/crates/kbf-driver-container".into(),
    }
}

/// [`systemd_run_args`] for `target` with the runner as these tests set it, as text.
fn args_of(target: &Target, home: &OsStr) -> Vec<OsString> {
    let built = built(target);
    systemd_run_args(&Unit {
        target,
        built: &built,
        uid: 1001,
        gid: 118,
        home,
        path: OsStr::new("/usr/bin:/bin"),
        manifest: BUSYBOX_AMD64,
    })
}

/// The arguments every target's unit gets, before its own.
fn common(name: &str) -> Vec<String> {
    [
        "systemd-run",
        "--quiet",
        "--wait",
        "--collect",
        "--pipe",
        &format!("--unit=kbf-crun-tests-{name}"),
        "--slice=kbf-daemon.slice",
        "-p",
        "Delegate=yes",
        "-p",
        "OOMScoreAdjust=-900",
        "--uid=1001",
        "--gid=118",
        "--working-directory=/w/crates/kbf-driver-container",
        "-E",
        "HOME=/srv/runner",
        "-E",
        "PATH=/usr/bin:/bin",
        "-E",
        &format!("KBF_TEST_IMAGE=docker://docker.io/library/busybox@{BUSYBOX_AMD64}"),
        "-E",
        "KBF_TEST_INDEX_IMAGE=docker://docker.io/library/busybox@sha256:\
         bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn every_target_s_unit_is_delegated_with_the_daemon_s_settings() {
    // Catches any of the unit's settings dropped or changed: Delegate=yes (the tests
    // cannot set up their cgroup), OOMScoreAdjust=-900 (the swap tests see another OOM
    // score than an action inherits), the slice, the runner's uid and gid (the tests
    // would run as root), the package directory, an
    // environment variable, the manifest and index images swapped, a unit that does
    // not wait or pass the exit status through, and the ignored tests left out.
    let home = OsStr::new("/srv/runner");
    for target in TARGETS {
        let mut want = common(target.name);
        want.push(format!("/w/target/debug/deps/{}-0f1e", target.name));
        want.push("--include-ignored".to_owned());
        if target.nocapture {
            want.push("--nocapture".to_owned());
        }
        let got: Vec<String> = args_of(target, home)
            .iter()
            .map(|a| a.to_str().expect("utf-8").to_owned())
            .collect();
        assert_eq!(got, want, "{}", target.name);
    }
}

#[test]
fn nocapture_goes_to_the_timing_targets_only_and_every_target_runs_in_order() {
    // Catches --nocapture given to every unit or to none (the timing targets' timings
    // would be lost or every log flooded), and a target dropped from the run, or run
    // in another order than the script ran them; and no unit given a runtime
    // environment a node does not have (no XDG_RUNTIME_DIR, no containers.conf).
    let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [
            "crun",
            "crun_memory",
            "crun_killed",
            "crun_cache",
            "crun_env",
            "crun_credential",
            "oci_crun"
        ]
    );
    let with = |needle: &str| -> Vec<&'static str> {
        TARGETS
            .iter()
            .filter(|t| {
                args_of(t, OsStr::new("/h"))
                    .iter()
                    .any(|a| a.to_string_lossy().starts_with(needle))
            })
            .map(|t| t.name)
            .collect()
    };
    assert_eq!(with("CONTAINERS_CONF"), Vec::<&str>::new());
    assert_eq!(with("XDG_RUNTIME_DIR="), Vec::<&str>::new());
    assert_eq!(with("--nocapture"), ["crun_cache", "oci_crun"]);
    assert_eq!(with("Delegate=yes"), names);
}

#[test]
fn a_home_that_is_not_utf8_reaches_the_unit_as_it_is() {
    // Catches the environment passed through a lossy UTF-8 conversion.
    let home = OsStr::from_bytes(b"/srv/r\xffn");
    let args = args_of(&TARGETS[0], home);
    let mut want = OsString::from("HOME=");
    want.push(home);
    assert!(args.contains(&want), "{args:?}");
}
