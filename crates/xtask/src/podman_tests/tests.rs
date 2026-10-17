use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;

use super::*;

const D_ATTEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
const D_WINDOWS: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";
const D_ARM: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000003";
const D_ARM64: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000004";
const D_AMD64: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000005";
const D_AMD64_LATER: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000006";
const D_INDEX: &str = "sha256:00000000000000000000000000000000000000000000000000000000000000ff";

/// An image index as `podman manifest inspect` prints it, cut down: an attestation
/// first (`unknown/unknown`), a Windows amd64 image, linux/arm, then linux/arm64 and
/// linux/amd64, a second linux/amd64 entry after it, and linux/riscv64 with a digest
/// that is not SHA-256. No wanted entry is first, so taking the first entry is wrong
/// for every architecture.
fn index() -> String {
    let entry = |digest: &str, os: &str, arch: &str| {
        format!(
            r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":610,"digest":"{digest}","platform":{{"architecture":"{arch}","os":"{os}"}}}}"#
        )
    };
    let entries = [
        entry(D_ATTEST, "unknown", "unknown"),
        entry(D_WINDOWS, "windows", "amd64"),
        entry(D_ARM, "linux", "arm"),
        entry(D_ARM64, "linux", "arm64"),
        entry(D_AMD64, "linux", "amd64"),
        entry(D_AMD64_LATER, "linux", "amd64"),
        entry("sha512:00", "linux", "riscv64"),
    ];
    format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{}]}}"#,
        entries.join(",")
    )
}

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
fn the_linux_manifest_of_the_architecture_is_picked() {
    // Catches the first entry taken (the attestation), the OS not checked (Windows'
    // amd64), the architecture not checked, and the last match taken rather than the
    // first a pull resolves.
    assert_eq!(
        manifest_for(&index(), D_INDEX, "amd64"),
        Ok(D_AMD64.to_owned())
    );
    assert_eq!(
        manifest_for(&index(), D_INDEX, "arm64"),
        Ok(D_ARM64.to_owned())
    );
    assert_eq!(manifest_for(&index(), D_INDEX, "arm"), Ok(D_ARM.to_owned()));
}

#[test]
fn a_manifest_digest_equal_to_the_index_digest_is_refused() {
    // Catches the refusal dropped: the test of the image named by its index would run
    // the same image as the manifest test, and prove nothing.
    assert_eq!(
        manifest_for(&index(), D_AMD64, "amd64"),
        Err(Error::Failed(format!(
            "the linux/amd64 manifest's digest is the index's ({D_AMD64}); the index test \
             would mean nothing"
        )))
    );
    // Catches a refusal of any index digest that appears in the index: only the picked
    // manifest's digest counts.
    assert_eq!(
        manifest_for(&index(), D_AMD64, "arm64"),
        Ok(D_ARM64.to_owned())
    );
}

#[test]
fn no_usable_manifest_is_refused() {
    // Catches an architecture the index does not carry, an index without a manifest
    // list, and a digest that is not SHA-256 (the image reference would not resolve),
    // each passed on as some digest.
    let none = |arch: &str| {
        Err(Error::Failed(format!(
            "no linux/{arch} manifest with a sha256 digest in {BUSYBOX_NAME}@{D_INDEX}"
        )))
    };
    assert_eq!(manifest_for(&index(), D_INDEX, "s390x"), none("s390x"));
    assert_eq!(manifest_for(&index(), D_INDEX, "riscv64"), none("riscv64"));
    assert_eq!(manifest_for("{}", D_INDEX, "amd64"), none("amd64"));
    // Catches output that is not JSON taken for an empty index.
    let e = manifest_for("Error: no such manifest", D_INDEX, "amd64").expect_err("not JSON");
    assert!(
        e.to_string().starts_with("the image index is not JSON: "),
        "{e}"
    );
}

/// A scratch directory for one unit test, empty, with a short path (a socket path must
/// fit in 108 bytes).
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

#[test]
fn the_wait_ends_on_a_socket_and_only_on_a_socket() {
    let dir = scratch("socket");
    let bus = dir.join("bus");
    // Catches a wait that never looks again: the socket appears after the first look.
    let late = bus.clone();
    let maker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        UnixListener::bind(late).expect("bind")
    });
    assert_eq!(
        wait_for_socket(&bus, 200, Duration::from_millis(10)),
        Ok(())
    );
    drop(maker.join().expect("maker"));
    // Already there: found at once.
    assert_eq!(wait_for_socket(&bus, 1, Duration::from_secs(60)), Ok(()));
    // Catches the bus missing passed over: the verb would go on without a user manager.
    let missing = dir.join("missing");
    assert_eq!(
        wait_for_socket(&missing, 3, Duration::from_millis(1)),
        Err(Error::Failed(format!(
            "no socket at {} after 3 looks 1ms apart",
            missing.display()
        )))
    );
    // Catches a check for any file in place of a socket.
    let file = dir.join("file");
    std::fs::write(&file, "").expect("file");
    assert!(wait_for_socket(&file, 2, Duration::from_millis(1)).is_err());
    let _ = std::fs::remove_dir_all(&dir);
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
        runtime_dir: Path::new("/run/user/1001"),
        manifest: D_AMD64,
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
        &format!("--unit=kbf-podman-tests-{name}"),
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
        "XDG_RUNTIME_DIR=/run/user/1001",
        "-E",
        &format!("KBF_TEST_IMAGE=docker://docker.io/library/busybox@{D_AMD64}"),
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
    // would run as root, with root's Podman store), the package directory, an
    // environment variable, the manifest and index images swapped, a unit that does
    // not wait or pass the exit status through, and the ignored tests left out.
    let home = OsStr::new("/srv/runner");
    for target in TARGETS {
        let mut want = common(target.name);
        if target.containers_conf {
            want.extend(
                [
                    "-E",
                    "CONTAINERS_CONF_OVERRIDE=/w/crates/kbf-driver-container/tests/fixtures/\
                     containers-override.conf",
                ]
                .map(str::to_owned),
            );
        }
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
fn the_containers_conf_override_and_nocapture_go_to_one_target_each() {
    // Catches CONTAINERS_CONF_OVERRIDE given to every unit (the other targets would
    // run under a node's containers.conf and test something else) or to none (the
    // podman_env tests would pass without the override), and the same for the
    // podman_cache timings' --nocapture; and a target dropped from the run, or run in
    // another order than the script ran them.
    let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [
            "podman",
            "podman_memory",
            "podman_killed",
            "podman_cache",
            "podman_env",
            "podman_credential",
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
    assert_eq!(with("CONTAINERS_CONF_OVERRIDE="), ["podman_env"]);
    assert_eq!(with("--nocapture"), ["podman_cache", "oci_crun"]);
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

/// Catches a default runtime directory other than the one systemd gives the user, so
/// the verb waits for a bus that never appears there (and podman looks in the wrong
/// place) on a runner, which passes no `--runtime-dir`.
#[test]
fn the_default_runtime_dir_is_the_user_s_under_run_user() {
    assert_eq!(default_runtime_dir(1001), Path::new("/run/user/1001"));
    assert_eq!(default_runtime_dir(0), Path::new("/run/user/0"));
}
